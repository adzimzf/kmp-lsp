//! `rename` feature — shared rename logic behind thin backend adapters.
//!
//! Entry points: [`prepare_rename_impl`] and [`rename_impl`]. The backend
//! adapter only unwraps LSP params; this module handles CST-verified local
//! (`local_scope_occurrences`) and cross-file (`verified_references_for`)
//! rename, refusal-as-error on ambiguous/library/override identities, and
//! edit construction.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use crate::features::java_references::JavaRename;
use crate::features::references::verified_references_for;
use crate::features::text_utils::is_keyword_for_file;
use crate::indexer::{
    classify_cursor, local_scope_occurrences, resolve_identity, Indexer, NavigationSource,
};

pub(crate) async fn prepare_rename_impl(
    indexer: &Indexer,
    uri: &Url,
    pos: Position,
) -> Result<Option<PrepareRenameResponse>> {
    let (word, range) = match indexer.word_and_range_at(uri, pos) {
        Some(word_and_range) => word_and_range,
        None => return Ok(None),
    };

    if is_keyword_for_file(&word, uri.path()) {
        return Ok(None);
    }

    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range,
        placeholder: word,
    }))
}

fn workspace_edit_from_locations(locations: &[Location], new_name: &str) -> WorkspaceEdit {
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for location in locations {
        changes
            .entry(location.uri.clone())
            .or_default()
            .push(TextEdit {
                range: location.range,
                new_text: new_name.to_owned(),
            });
    }
    for edits in changes.values_mut() {
        edits.sort_by_key(|edit| Reverse(edit.range.start));
    }
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

/// One edit per file, from its first renamed name to its last, the text in
/// between kept — how jdtls answers a Java rename. A client applies it in one
/// step, and nothing in between can be left half-renamed. A file whose text
/// cannot be read gets one edit per name instead.
fn merged_workspace_edit(
    indexer: &Indexer,
    locations: &[Location],
    new_name: &str,
) -> WorkspaceEdit {
    let mut by_file: HashMap<Url, Vec<Range>> = HashMap::new();
    for location in locations {
        by_file
            .entry(location.uri.clone())
            .or_default()
            .push(location.range);
    }
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for (uri, mut ranges) in by_file {
        ranges.sort_by_key(|range| range.start);
        ranges.dedup();
        let edits = indexer
            .lines_for(&uri)
            .and_then(|lines| merged_edit(&lines, &ranges, new_name))
            .map(|edit| vec![edit])
            .unwrap_or_else(|| {
                ranges
                    .iter()
                    .rev()
                    .map(|range| TextEdit {
                        range: *range,
                        new_text: new_name.to_owned(),
                    })
                    .collect()
            });
        changes.insert(uri, edits);
    }
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

/// The single edit replacing `ranges` (sorted, single-line, not overlapping)
/// with `new_name` and keeping the text between them.
fn merged_edit(lines: &[String], ranges: &[Range], new_name: &str) -> Option<TextEdit> {
    let first = ranges.first()?;
    let last = ranges.last()?;
    let mut text = String::new();
    let mut at = first.start;
    for range in ranges {
        if range.start.line != range.end.line || range.start < at {
            return None;
        }
        text.push_str(&text_between(lines, at, range.start)?);
        text.push_str(new_name);
        at = range.end;
    }
    Some(TextEdit {
        range: Range::new(first.start, last.end),
        new_text: text,
    })
}

/// The text from `start` to `end` (UTF-16 positions), lines joined by `\n`.
fn text_between(lines: &[String], start: Position, end: Position) -> Option<String> {
    let byte_at = |position: Position| -> Option<usize> {
        let line = lines.get(position.line as usize)?;
        let mut units = 0;
        for (byte, character) in line.char_indices() {
            if units >= position.character {
                return Some(byte);
            }
            units += character.len_utf16() as u32;
        }
        (units >= position.character).then_some(line.len())
    };
    let (from, to) = (byte_at(start)?, byte_at(end)?);
    if start.line == end.line {
        return lines[start.line as usize].get(from..to).map(str::to_owned);
    }
    let mut text = lines[start.line as usize][from..].to_owned();
    for line in &lines[start.line as usize + 1..end.line as usize] {
        text.push('\n');
        text.push_str(line);
    }
    text.push('\n');
    text.push_str(&lines[end.line as usize][..to]);
    Some(text)
}

/// Build a user-facing rename refusal. Rename refusals are errors BY DESIGN
/// (see the design doc's Global Constraints) — never `Ok(None)` — so the
/// reason surfaces to the user (e.g. the Helix status line) instead of
/// silently doing nothing. `panic_safe` (the only wrapper between this
/// function and the LSP transport, see `src/backend/panic_guard.rs`) forwards
/// any `Result::Err` unchanged, so no adapter change is needed for this to
/// reach the client.
///
/// `InvalidRequest` matches the only existing `tower_lsp::jsonrpc::Error`
/// construction in this codebase (`panic_guard.rs`'s raw struct literal for
/// panics, which uses `InternalError` — not appropriate here since this is an
/// expected business-rule refusal, not a bug). No other user-facing refusal
/// code exists in this codebase to match instead.
fn refusal(reason: &str) -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::InvalidRequest,
        message: reason.to_owned().into(),
        data: None,
    }
}

pub(crate) async fn rename_impl(
    indexer: &Arc<Indexer>,
    uri: &Url,
    pos: Position,
    new_name: &str,
) -> Result<Option<WorkspaceEdit>> {
    // Local fast path: a real CST subtree walk over the enclosing
    // function/lambda body. Never refuses, never crosses the scope boundary.
    if let Some(locations) = local_scope_occurrences(indexer, uri, pos) {
        return Ok(Some(workspace_edit_from_locations(&locations, new_name)));
    }

    // A Java type or method: what find-references finds, with the names
    // that have to change along (see `java_rename`).
    match crate::features::java_references::java_rename(indexer, uri, pos).await {
        Some(JavaRename::Names(locations)) if locations.is_empty() => return Ok(None),
        Some(JavaRename::Names(locations)) => {
            return Ok(Some(merged_workspace_edit(indexer, &locations, new_name)))
        }
        Some(JavaRename::Refused(reason)) => return Err(refusal(reason)),
        None => {}
    }

    let Some(symbol) = classify_cursor(indexer, uri, pos) else {
        return Ok(None);
    };

    let identity = resolve_identity(&symbol, indexer, uri);
    let NavigationSource::CstResolved(definitions) = identity else {
        return Err(refusal(
            "identity is ambiguous — could not resolve a single definition",
        ));
    };
    // A same-arity extension collision (two unrelated `Modifier.weight`
    // declarations, say) reaches this refusal BY DESIGN since PR #321:
    // `resolve_identity` correctly returns the whole overload set rather than
    // one arbitrary member of it, and silently renaming an arbitrarily chosen
    // one of two genuinely distinct declarations is the unsound behaviour that
    // PR replaced. Listing both candidates in the message would not help --
    // an LSP error is surfaced as a toast with no navigation affordance -- so a
    // real disambiguation UX needs a client-side picker this server has no
    // protocol hook for. Reviewed and kept as-is, 2026-09-16.
    //
    // The 2026-09-16 extension-registry-follow-ups cluster raised how OFTEN
    // this fires, not whether it should: its Task 2 widened extension-scope
    // recognition (default-import packages now count), so more real
    // candidates reach `resolve_extension_in_scope` per receiver, and a
    // real-corpus measurement across that whole cluster showed FilteredCandidate
    // rising by 449 (see that plan's final review) -- some fraction of which
    // is this refusal firing on receivers it previously never reached at all.
    // The reasoning above still holds; only its frequency changed.
    if definitions.len() != 1 {
        return Err(refusal(
            "identity is ambiguous — matches more than one definition",
        ));
    }
    if indexer.is_library_uri(&definitions[0].uri) {
        return Err(refusal(
            "defined in a library — cannot rename a library symbol",
        ));
    }

    let qualifier = None; // rename's cursor site has no dot-qualifier context today; matches prior behavior.
    let (verified, _query_declaring_type, _query_declaring_type_uri) = verified_references_for(
        &symbol.name,
        qualifier,
        uri,
        pos,
        true,
        indexer,
        usize::MAX,
        true, // detect_reverse_overrides: rename needs proven_overrides populated
    )
    .await;

    if !verified.proven_overrides.is_empty() {
        return Err(refusal(
            "renaming an override relationship is not supported — rename the exact \
             declaration you need",
        ));
    }

    let edit_locations: Vec<Location> = verified
        .kept
        .into_iter()
        .map(|source| match source {
            NavigationSource::CstResolved(location) => location,
            NavigationSource::NameScan(location) => location,
        })
        .collect();
    if edit_locations.is_empty() {
        return Ok(None);
    }

    Ok(Some(workspace_edit_from_locations(
        &edit_locations,
        new_name,
    )))
}

#[cfg(test)]
#[path = "rename_tests.rs"]
mod tests;

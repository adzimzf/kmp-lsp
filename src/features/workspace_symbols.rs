//! Workspace symbol search — index-first with rg cold-start fallback.
//!
//! Entry point: [`compute_workspace_symbols`].
//! Bounds: `SymbolIndex + SearchAccess`.

use std::sync::Arc;

use crate::rg;
use crate::types::{FileData, SymbolEntry};
use tower_lsp::lsp_types::{Location, SymbolInformation, Url};

use super::traits::{SearchAccess, SymbolIndex};

/// Maximum results returned from the index scan.
const WORKSPACE_SYMBOL_CAP: usize = 512;

/// Throttle counter for [`collect_index_symbols`]'s URI-parse warning — see
/// [`crate::util::throttled_warn`].
static INDEXED_FILE_URI_PARSE_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Timeout for the rg cold-start fallback.
///
/// Note: this bounds how long `compute_workspace_symbols` *awaits* the
/// `spawn_blocking` task.  Dropping the `JoinHandle` detaches the task —
/// the underlying `rg` child process may continue briefly until it exits
/// naturally or is reaped by the OS.  The timeout still prevents the LSP
/// handler from blocking indefinitely.
const RG_FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Search workspace symbols by `query`, returning up to `WORKSPACE_SYMBOL_CAP` results.
///
/// Index-first: scans all indexed files for matching symbols.
/// rg fallback: fired **only on cold start** (index not yet populated) so that
/// editors get results before the initial scan completes.  When the index already
/// has files, an empty result means the symbol is genuinely absent — no rg scan.
/// The fallback is capped at [`RG_FALLBACK_TIMEOUT`] to prevent runaway scans on
/// large repos.
pub(crate) async fn compute_workspace_symbols(
    query_str: String,
    index: &(impl SymbolIndex + SearchAccess),
) -> Vec<SymbolInformation> {
    let query = WorkspaceSymbolQuery::new(query_str);
    let (mut results, index_populated) = collect_index_symbols(&query, index);
    if results.is_empty() && !index_populated {
        results = rg_symbol_search(&query, index).await;
    }
    results
}

// ─── Index scan ──────────────────────────────────────────────────────────────

/// Returns `(matching_symbols, index_populated)`.
///
/// `index_populated` is `true` when at least one file has been indexed,
/// regardless of whether any symbol matched the query.
fn collect_index_symbols(
    query: &WorkspaceSymbolQuery,
    index: &impl SymbolIndex,
) -> (Vec<SymbolInformation>, bool) {
    let mut results = Vec::new();
    let mut index_populated = false;
    let mut f = |uri_str: &str, data: &Arc<FileData>| {
        index_populated = true;
        let Some(uri) = parse_uri(uri_str) else {
            // `uri_str` is a key of `Indexer::files`/`jar_files`, always
            // inserted as `Url::to_string()` of a URI this process produced
            // (or a `Url::from_file_path` round-trip for the same). Neither
            // parse should ever fail here — a hit means the key is
            // corrupted, and every symbol in this file silently drops out
            // of workspace-symbol search.
            crate::util::throttled_warn(&INDEXED_FILE_URI_PARSE_FAILURES, 5, || {
                format!(
                    "workspace_symbols: indexed file key {uri_str:?} is not a valid URI or file \
                     path — its {} symbol(s) are excluded from every workspace-symbol search",
                    data.symbols.len(),
                )
            });
            return true;
        };
        for symbol in &data.symbols {
            if !query.matches(symbol) {
                continue;
            }
            results.push(symbol_information(&uri, symbol));
            if results.len() >= WORKSPACE_SYMBOL_CAP {
                return false;
            }
        }
        true
    };
    index.for_each_indexed_file(&mut f);
    results.sort_by(|a, b| a.name.cmp(&b.name));
    (results, index_populated)
}

// ─── rg fallback ─────────────────────────────────────────────────────────────

async fn rg_symbol_search(
    query: &WorkspaceSymbolQuery,
    index: &impl SearchAccess,
) -> Vec<SymbolInformation> {
    if !query.allows_rg_fallback() {
        return vec![];
    }
    // Cold-start heuristic: if the query matches a well-known Kotlin/JVM
    // built-in type name, skip the rg scan.  A project *could* technically
    // declare a class named `String`, but that would be highly unusual and
    // any such file would be indexed shortly after cold start.  Accepting
    // this tradeoff avoids a pointless full-workspace scan for names that
    // are almost certainly from `kotlin.*` or `java.lang.*`.
    {
        use crate::features::text_utils::KOTLIN_BUILTIN_TYPES;
        if KOTLIN_BUILTIN_TYPES
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&query.name))
        {
            log::trace!("workspace_symbols: skip stdlib type {}", query.name);
            return vec![];
        }
    }
    let (workspace_root, source_roots, ignore_matcher) = index.rg_scope_for_path(None);
    let name = query.name.clone();
    let task = tokio::task::spawn_blocking(move || {
        rg::rg_find_definition(
            &name,
            workspace_root.as_deref(),
            &source_roots,
            ignore_matcher.as_deref(),
        )
    });
    let locations = match tokio::time::timeout(RG_FALLBACK_TIMEOUT, task).await {
        Ok(Ok(locs)) => locs,
        Ok(Err(_)) | Err(_) => {
            log::warn!("workspace_symbols: rg cold-start fallback timed out or failed");
            vec![]
        }
    };

    locations
        .into_iter()
        .map(|loc| rg_symbol_information(query.name.clone(), loc))
        .collect()
}

// ─── Query type ──────────────────────────────────────────────────────────────

/// Parsed and lowercased workspace symbol query.
#[derive(Clone)]
struct WorkspaceSymbolQuery {
    raw: String,
    qualifier: Option<String>,
    name: String,
    /// The name as typed, before lowercasing: its capitals mark camel-case
    /// parts.
    typed_name: String,
}

impl WorkspaceSymbolQuery {
    fn new(query: String) -> Self {
        let raw = query.to_lowercase();
        if let Some(dot) = query.rfind('.') {
            return Self {
                qualifier: Some(raw[..dot].to_owned()),
                name: raw[dot + 1..].to_owned(),
                typed_name: query[dot + 1..].to_owned(),
                raw,
            };
        }
        Self {
            name: raw.clone(),
            typed_name: query,
            raw,
            qualifier: None,
        }
    }

    fn matches(&self, symbol: &SymbolEntry) -> bool {
        if self.raw.is_empty() {
            return true;
        }
        // `self.name` and `self.raw` are pre-lowercased; use allocation-free helpers.
        use crate::features::text_utils::contains_ignore_ascii_case;
        let name_matches = name_matches(&self.typed_name, &symbol.name);
        if let Some(qualifier) = self.qualifier.as_deref() {
            return name_matches && contains_ignore_ascii_case(&symbol.detail, qualifier);
        }
        name_matches
    }

    fn allows_rg_fallback(&self) -> bool {
        !self.raw.is_empty() && self.qualifier.is_none()
    }
}

/// Whether `name` is one a workspace-symbol `query` asks for, as jdtls
/// matches them: a query in lower case is a prefix of the name, ignoring
/// case (`circle` finds `CircleTest`, not `SemiCircleTest`); a query with
/// capitals is also read as camel-case parts, each the start of one of the
/// name's parts in a row (`CT` finds `CircleTest`). The parts start at the
/// name's first one, or at any of its parts when the query's first part is
/// more than an initial (`CirTe` finds `SemiCircleTest`; `CT` does not). A single part is
/// only ever a prefix.
fn name_matches(query: &str, name: &str) -> bool {
    let is_prefix = name
        .get(..query.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(query));
    if is_prefix {
        return true;
    }
    let has_capital = query
        .chars()
        .any(|character| character.is_ascii_uppercase());
    let query_parts = camel_case_parts(query);
    if !has_capital || query_parts.len() < 2 {
        return false;
    }
    let name_parts = camel_case_parts(name);
    let first_part_is_an_initial = query_parts.first().is_some_and(|part| part.len() == 1);
    let last_start = if first_part_is_an_initial {
        0
    } else {
        name_parts.len().saturating_sub(1)
    };
    (0..=last_start).any(|start| parts_match_from(&query_parts, &name_parts, start))
}

/// Whether each of `query_parts` starts the name part at the same distance
/// from `start`.
fn parts_match_from(query_parts: &[String], name_parts: &[String], start: usize) -> bool {
    let Some(name_parts) = name_parts.get(start..) else {
        return false;
    };
    if query_parts.len() > name_parts.len() {
        return false;
    }
    query_parts
        .iter()
        .zip(name_parts)
        .all(|(query_part, name_part)| name_part.starts_with(query_part.as_str()))
}

/// `SemiCircleTest` as `Semi`, `Circle`, `Test`: a part starts at each
/// capital, digit run or `_`-separated word.
fn camel_case_parts(name: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for character in name.chars() {
        if character == '_' {
            parts.push(String::new());
            continue;
        }
        let starts_part = character.is_ascii_uppercase() || parts.is_empty();
        if starts_part {
            parts.push(character.to_string());
        } else if let Some(last) = parts.last_mut() {
            last.push(character);
        }
    }
    parts.retain(|part| !part.is_empty());
    parts
}

// ─── LSP conversion helpers ──────────────────────────────────────────────────

fn parse_uri(uri_str: &str) -> Option<Url> {
    Url::parse(uri_str)
        .ok()
        .or_else(|| Url::from_file_path(uri_str).ok())
}

#[allow(deprecated)] // `deprecated` superseded by `tags` in LSP 3.16+
fn symbol_information(uri: &Url, symbol: &SymbolEntry) -> SymbolInformation {
    SymbolInformation {
        name: symbol.name.clone(),
        kind: symbol.kind,
        tags: None,
        deprecated: None,
        location: Location {
            uri: uri.clone(),
            range: symbol.selection_range,
        },
        container_name: (!symbol.detail.is_empty()).then(|| symbol.detail.clone()),
    }
}

#[allow(deprecated)]
fn rg_symbol_information(name: String, location: Location) -> SymbolInformation {
    SymbolInformation {
        name,
        kind: tower_lsp::lsp_types::SymbolKind::FILE,
        tags: None,
        deprecated: None,
        location,
        container_name: Some("rg fallback".to_string()),
    }
}

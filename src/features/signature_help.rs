//! Signature help feature — extracts the active call site via CST and returns parameter info.
//!
//! The CST (`cst_call_info`) is authoritative: it walks up the live tree-sitter parse tree to
//! find the enclosing `call_expression`, counts `value_argument` children for `active_param`,
//! and handles multiline calls naturally.
//!
//! When the closing `)` is absent (live typing mid-argument) the CST cannot form a
//! `call_expression`, so `call_info_at` uses a text-scan fallback on the current line.
//!
//! When the cursor is inside a nested call (e.g. `setOf()`) whose signature cannot be resolved,
//! the outer call is tried as a fallback so the user still sees helpful parameter info.
//!
//! Java calls are answered by [`java_signature_help`] instead: the callee is
//! resolved as go-to-definition resolves it (receiver type, imports, overloads
//! narrowed by the argument types), and every overload declared beside it is
//! listed, the resolved one active — as jdtls answers.

use tower_lsp::lsp_types::{
    ParameterInformation, ParameterLabel, Position, SignatureHelp, SignatureInformation, Url,
};

use tower_lsp::lsp_types::Location;
use tree_sitter::{Node, Point};

use super::traits::{LiveTreeAccess, SignatureIndex};
use crate::features::call_hierarchy::callee_name;
use crate::features::parameter_hints::callee_declarations;
use crate::features::selection_range::point_of;
use crate::indexer::{split_params_at_depth_zero, Indexer};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};
use crate::types::SymbolEntry;

/// Compute signature help for the call under the cursor at `pos` in `uri`.
///
/// Returns `None` when:
/// - no live parse tree exists for the file (not yet opened/edited),
/// - the cursor is not inside a `call_expression` (e.g. inside a trailing lambda body), or
/// - the function name cannot be resolved to a known signature.
pub(crate) fn compute_signature_help(
    uri: &Url,
    pos: Position,
    index: &(impl SignatureIndex + LiveTreeAccess),
) -> Option<SignatureHelp> {
    let ci = index.call_info_at(pos, uri)?;

    if let Some(params_text) =
        index.find_fun_signature_with_receiver(uri, &ci.fn_name, ci.qualifier.as_deref())
    {
        return build_signature_help(&ci.fn_name, &params_text, ci.active_param);
    }

    // Inner call's signature not found (e.g. stdlib overloaded function like `setOf`).
    // Try the enclosing call expression so the user still sees the outer parameter info.
    let outer = index.outer_call_info_at(pos, uri)?;
    let params_text =
        index.find_fun_signature_with_receiver(uri, &outer.fn_name, outer.qualifier.as_deref())?;
    build_signature_help(&outer.fn_name, &params_text, outer.active_param)
}

/// Signature help for the Java call at `site` (see [`java_call_site`]);
/// `None` when its callee doesn't resolve.
pub(crate) async fn java_signature_help(
    indexer: &Indexer,
    uri: &Url,
    site: JavaCallSite,
) -> Option<SignatureHelp> {
    let declarations = callee_declarations(indexer, uri, site.callee).await;
    let resolved = declarations.first()?;
    let (overloads, active) = overloads_of(indexer, resolved)?;
    let signatures: Vec<SignatureInformation> = overloads
        .iter()
        .map(|symbol| java_signature(symbol, site.argument))
        .collect();
    let active_parameter = signatures[active].active_parameter;
    Some(SignatureHelp {
        signatures,
        active_signature: Some(u32::try_from(active).ok()?),
        active_parameter,
    })
}

/// A Java call around the cursor: where its callee is named, and which of
/// its arguments the cursor is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JavaCallSite {
    pub(crate) callee: Position,
    pub(crate) argument: u32,
}

/// The innermost Java call whose argument list holds `pos` (after its `(`,
/// up to its `)`). `None` for any other language.
pub(crate) fn java_call_site(indexer: &Indexer, uri: &Url, pos: Position) -> Option<JavaCallSite> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let doc = indexer.live_doc_or_parse(uri)?;
    let point = point_of(&doc.bytes, pos)?;
    let mut node = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point);
    while let Some(current) = node {
        if current.kind() == "argument_list" && holds(current, point) {
            if let Some(callee) = current.parent().and_then(called_name) {
                let starts = line_starts(&doc.bytes);
                return Some(JavaCallSite {
                    callee: ts_pos_to_lsp(callee.start_position(), &starts, &doc.bytes),
                    argument: argument_index(current, point),
                });
            }
        }
        node = current.parent();
    }
    None
}

/// Whether `point` is inside `arguments`' parentheses: after the `(`, at the
/// `)` at the latest.
fn holds(arguments: Node<'_>, point: Point) -> bool {
    let after_open = arguments.start_position() < point;
    let closed = arguments
        .child(u32::try_from(arguments.child_count().saturating_sub(1)).unwrap_or(0))
        .filter(|last| last.kind() == ")" && !last.is_missing());
    let before_close = closed.map_or(point <= arguments.end_position(), |close| {
        point <= close.start_position()
    });
    after_open && before_close
}

/// The name a call node calls: the method of an invocation, the type of a
/// `new`, the `this` / `super` of a constructor call.
pub(crate) fn called_name(call: Node<'_>) -> Option<Node<'_>> {
    match call.kind() {
        "explicit_constructor_invocation" => call.child_by_field_name("constructor"),
        _ => callee_name(call),
    }
}

/// How many `,` of `arguments` come before `point`.
fn argument_index(arguments: Node<'_>, point: Point) -> u32 {
    let mut cursor = arguments.walk();
    let commas = arguments
        .children(&mut cursor)
        .filter(|child| child.kind() == "," && child.end_position() <= point)
        .count();
    u32::try_from(commas).unwrap_or(u32::MAX)
}

/// The methods (or constructors) declared beside the one at `resolved` under
/// its name, in source order, and the index of the resolved one.
fn overloads_of(indexer: &Indexer, resolved: &Location) -> Option<(Vec<SymbolEntry>, usize)> {
    let file = indexer
        .files
        .get(resolved.uri.as_str())
        .map(|file| std::sync::Arc::clone(file.value()))
        .or_else(|| {
            indexer
                .jar_files
                .get(resolved.uri.as_str())
                .map(|file| std::sync::Arc::clone(file.value()))
        })?;
    let target = file
        .symbols
        .iter()
        .find(|symbol| symbol.selection_range == resolved.range)?;
    if !matches!(
        target.kind,
        tower_lsp::lsp_types::SymbolKind::METHOD | tower_lsp::lsp_types::SymbolKind::CONSTRUCTOR
    ) {
        return None;
    }
    let mut overloads: Vec<SymbolEntry> = file
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == target.kind
                && symbol.name == target.name
                && symbol.container == target.container
        })
        .cloned()
        .collect();
    overloads.sort_by_key(|symbol| symbol.range.start);
    let active = overloads
        .iter()
        .position(|symbol| symbol.selection_range == target.selection_range)?;
    Some((overloads, active))
}

/// `assertThat(String actual)` for `assertThat(@Nullable String actual)`,
/// the cursor's argument active (the last parameter for any argument past it,
/// as a varargs parameter takes them all).
fn java_signature(symbol: &SymbolEntry, argument: u32) -> SignatureInformation {
    let params = symbol.params.trim();
    let params = params.strip_prefix('(').unwrap_or(params);
    let params = params.strip_suffix(')').unwrap_or(params);
    let parameters: Vec<String> = split_params_at_depth_zero(params)
        .into_iter()
        .map(java_parameter_label)
        .filter(|label| !label.is_empty())
        .collect();
    let label = format!("{}({})", symbol.name, parameters.join(", "));
    let active = argument.min(u32::try_from(parameters.len().saturating_sub(1)).unwrap_or(0));
    SignatureInformation {
        label,
        documentation: None,
        parameters: Some(
            parameters
                .into_iter()
                .map(|parameter| ParameterInformation {
                    label: ParameterLabel::Simple(parameter),
                    documentation: None,
                })
                .collect(),
        ),
        active_parameter: Some(active),
    }
}

/// A Java parameter as signature help shows it: its type and name, without
/// annotations or `final`, whitespace collapsed.
pub(crate) fn java_parameter_label(parameter: &str) -> String {
    strip_declaration_prefix(parameter)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` past its leading annotations (`@Nullable`, `@SuppressWarnings("x")`),
/// modifiers (`public static final`) and type parameters (`<T>`): from a
/// declaration's text, where its type starts.
pub(crate) fn strip_declaration_prefix(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if let Some(annotated) = rest.strip_prefix('@') {
            let name_end = annotated
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '$'))
                .unwrap_or(annotated.len());
            rest = annotated[name_end..].trim_start();
            if rest.starts_with('(') {
                rest = rest[closing_end(rest, '(', ')')..].trim_start();
            }
        } else if rest.starts_with('<') {
            rest = rest[closing_end(rest, '<', '>')..].trim_start();
        } else if let Some(word) = rest
            .split_whitespace()
            .next()
            .filter(|word| is_modifier(word))
        {
            rest = rest[word.len()..].trim_start();
        } else {
            return rest;
        }
    }
}

fn is_modifier(word: &str) -> bool {
    matches!(
        word,
        "public"
            | "protected"
            | "private"
            | "static"
            | "final"
            | "abstract"
            | "synchronized"
            | "native"
            | "default"
            | "transient"
            | "volatile"
            | "strictfp"
    )
}

/// The byte just past the `close` matching the `open` that `text` starts with.
fn closing_end(text: &str, open: char, close: char) -> usize {
    let mut depth = 0usize;
    for (index, c) in text.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return index + c.len_utf8();
            }
        }
    }
    text.len()
}

fn build_signature_help(
    fn_name: &str,
    params_text: &str,
    active_param: u32,
) -> Option<SignatureHelp> {
    let raw = params_text.trim_matches(|c| c == '(' || c == ')');
    let param_parts: Vec<String> = split_params_at_depth_zero(raw)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let parameters: Vec<ParameterInformation> = param_parts
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(p.clone()),
            documentation: None,
        })
        .collect();
    let label = format!("{}({})", fn_name, param_parts.join(", "));
    let active_param = active_param.min(parameters.len().saturating_sub(1) as u32);
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: None,
            parameters: Some(parameters),
            active_parameter: Some(active_param),
        }],
        active_signature: Some(0),
        active_parameter: Some(active_param),
    })
}

#[cfg(test)]
#[path = "signature_help_tests.rs"]
mod tests;

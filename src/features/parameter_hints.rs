//! Parameter-name inlay hints for Java calls: `a.add(1)` shows as
//! `a.add(number: 1)`.
//!
//! Like jdtls's default (`java.inlayHints.parameterNames.enabled: literals`),
//! only literal arguments get a hint — numbers, strings, characters,
//! booleans, `null` and class literals — since a variable usually names what
//! it holds already. The callee is resolved as go-to-definition resolves it
//! (overloads narrowed by the argument types), so `list.add(0, x)` names
//! `index` and `element` while `list.add(x)` names `e`.
//!
//! [`literal_calls`] reads the call sites from the tree, [`callee_parameters`]
//! resolves each callee and [`hints_for`] turns its parameters into hints.

use tower_lsp::lsp_types::{
    GotoDefinitionResponse, InlayHint, InlayHintKind, InlayHintLabel, Location, Position, Range,
    Url,
};

use crate::backend::cursor::CursorContext;
use crate::features::definition::find_definition;
use crate::indexer::{
    declared_parameters_at, literal_argument_calls, narrow_java_overloads, parameter_for_argument,
    DeclaredParameter, Indexer,
};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};

/// Parameter-name hints for the literal arguments of the Java calls in
/// `range` of `uri`. Empty for any other language.
pub(crate) async fn java_parameter_hints(
    indexer: &Indexer,
    uri: &Url,
    range: Range,
) -> Vec<InlayHint> {
    let mut hints = Vec::new();
    for call in literal_calls(indexer, uri, range) {
        if let Some(parameters) = callee_parameters(indexer, uri, call.callee).await {
            hints.extend(hints_for(&call, &parameters));
        }
    }
    hints
}

/// The parameters of the method or constructor that the call named at
/// `callee` resolves to, as go-to-definition resolves it. `None` when it
/// doesn't resolve, or resolves to overloads whose parameter names differ.
pub(crate) async fn callee_parameters(
    indexer: &Indexer,
    uri: &Url,
    callee: Position,
) -> Option<Vec<DeclaredParameter>> {
    let locations = callee_declarations(indexer, uri, callee).await;
    let mut signatures = locations
        .iter()
        .filter_map(|location| declared_parameters_at(indexer, location));
    let first = signatures.next()?;
    signatures.all(|other| other == first).then_some(first)
}

/// The declarations the call named at `callee` resolves to, as
/// go-to-definition resolves it, narrowed to the overloads its arguments fit.
pub(crate) async fn callee_declarations(
    indexer: &Indexer,
    uri: &Url,
    callee: Position,
) -> Vec<Location> {
    let Some(ctx) = CursorContext::build(indexer, uri, callee) else {
        return Vec::new();
    };
    let locations = match find_definition(&ctx, indexer, uri, callee).await {
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location::new(link.target_uri, link.target_selection_range))
            .collect(),
        None => Vec::new(),
    };
    // `this(..)` resolves to its class: narrowing finds the constructor.
    narrow_java_overloads(indexer, uri, callee, locations)
}

/// A call that passes literals inside the requested range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiteralCall {
    /// Where to ask go-to-definition for the callee.
    pub(crate) callee: Position,
    pub(crate) argument_count: usize,
    /// Each literal argument's index and position.
    pub(crate) literals: Vec<(usize, Position)>,
}

/// The calls in the Java file `uri` that pass a literal on a line in `range`.
/// Empty for any other language.
pub(crate) fn literal_calls(indexer: &Indexer, uri: &Url, range: Range) -> Vec<LiteralCall> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return Vec::new();
    }
    let Some(doc) = indexer.live_doc_or_parse(uri) else {
        return Vec::new();
    };
    let starts = line_starts(&doc.bytes);
    let position_of = |point| ts_pos_to_lsp(point, &starts, &doc.bytes);
    let rows = range.start.line as usize..=range.end.line as usize;
    literal_argument_calls(doc.tree.root_node(), rows)
        .into_iter()
        .map(|call| LiteralCall {
            callee: position_of(call.callee),
            argument_count: call.argument_count,
            literals: call
                .literals
                .iter()
                .map(|literal| (literal.index, position_of(literal.start)))
                .collect(),
        })
        .collect()
}

/// The hints for `call`'s literals, given the parameters of the declaration
/// it calls. None when the parameters can't take the call's arguments.
pub(crate) fn hints_for(call: &LiteralCall, parameters: &[DeclaredParameter]) -> Vec<InlayHint> {
    call.literals
        .iter()
        .filter_map(|(index, position)| {
            let parameter = parameter_for_argument(parameters, call.argument_count, *index)?;
            Some(parameter_hint(*position, &parameter.name))
        })
        .collect()
}

fn parameter_hint(position: Position, name: &str) -> InlayHint {
    InlayHint {
        position,
        label: InlayHintLabel::String(format!("{name}:")),
        kind: Some(InlayHintKind::PARAMETER),
        text_edits: None,
        tooltip: None,
        padding_left: None,
        padding_right: Some(true),
        data: None,
    }
}

#[cfg(test)]
#[path = "parameter_hints_tests.rs"]
mod tests;

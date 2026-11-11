//! Call hierarchy: `textDocument/prepareCallHierarchy`,
//! `callHierarchy/incomingCalls` and `callHierarchy/outgoingCalls`.
//!
//! Incoming calls are the references to a function, minus mentions in
//! comments (`{@link #create()}`), grouped by the function, or else the type,
//! they occur in. Outgoing calls are the calls inside a function's body,
//! grouped by the function each resolves to. Finding the references and
//! resolving the calls is the backend's job (the references and definition
//! features); this module turns their locations into hierarchy items.

use std::collections::HashMap;

use tower_lsp::lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Location, Position,
    Range, SymbolKind, Url,
};
use tree_sitter::Node;

use crate::features::implementation::{is_class_kind, range_contains};
use crate::features::selection_range::point_of;
use crate::indexer::{Indexer, LiveDoc};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};
use crate::types::SymbolEntry;

/// The function declared at `position` in `uri`, if its name is there.
pub(crate) fn prepare_call_hierarchy(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<CallHierarchyItem> {
    let symbols = indexer.file_symbols(uri);
    let symbol = symbols.iter().find(|symbol| {
        is_callable(symbol.kind)
            && symbol.selection_range.start <= position
            && position <= symbol.selection_range.end
    })?;
    Some(call_item(uri, symbol))
}

/// The function declared at `location` (its name), if one is: where a
/// definition lookup from a call site lands.
pub(crate) fn callable_declared_at(
    indexer: &Indexer,
    location: &Location,
) -> Option<CallHierarchyItem> {
    let symbols = indexer.file_symbols(&location.uri);
    let symbol = symbols.iter().find(|symbol| {
        (is_callable(symbol.kind) || is_class_kind(symbol.kind))
            && range_contains(symbol.selection_range, location.range)
    })?;
    Some(call_item(&location.uri, symbol))
}

/// The calls among `references` (the references to `item`), grouped by the
/// function or type they are made from.
pub(crate) fn incoming_calls(
    indexer: &Indexer,
    item: &CallHierarchyItem,
    references: Vec<Location>,
) -> Vec<CallHierarchyIncomingCall> {
    let mut docs: HashMap<Url, Option<std::sync::Arc<LiveDoc>>> = HashMap::new();
    let mut symbols: HashMap<Url, Vec<SymbolEntry>> = HashMap::new();
    let mut calls: Vec<CallHierarchyIncomingCall> = Vec::new();
    for reference in references {
        if reference.uri == item.uri && reference.range == item.selection_range {
            continue;
        }
        let doc = docs
            .entry(reference.uri.clone())
            .or_insert_with(|| indexer.live_doc_or_parse(&reference.uri));
        if doc
            .as_deref()
            .is_some_and(|doc| in_comment(doc, reference.range.start))
        {
            continue;
        }
        let file_symbols = symbols
            .entry(reference.uri.clone())
            .or_insert_with(|| indexer.file_symbols(&reference.uri));
        let Some(caller) = enclosing_caller(file_symbols, reference.range) else {
            continue;
        };
        let from = call_item(&reference.uri, caller);
        match calls.iter_mut().find(|call| {
            call.from.uri == from.uri && call.from.selection_range == from.selection_range
        }) {
            Some(call) => call.from_ranges.push(reference.range),
            None => calls.push(CallHierarchyIncomingCall {
                from,
                from_ranges: vec![reference.range],
            }),
        }
    }
    calls
}

/// Groups `(call site, callee)` pairs by callee.
pub(crate) fn outgoing_calls(
    calls: Vec<(Range, CallHierarchyItem)>,
) -> Vec<CallHierarchyOutgoingCall> {
    let mut outgoing: Vec<CallHierarchyOutgoingCall> = Vec::new();
    for (site, callee) in calls {
        match outgoing.iter_mut().find(|call| {
            call.to.uri == callee.uri && call.to.selection_range == callee.selection_range
        }) {
            Some(call) => call.from_ranges.push(site),
            None => outgoing.push(CallHierarchyOutgoingCall {
                to: callee,
                from_ranges: vec![site],
            }),
        }
    }
    outgoing
}

/// The names of the calls made inside `item`'s declaration: the method name
/// of a Java invocation, the type of a `new`, the callee of a Kotlin or
/// Swift call.
pub(crate) fn call_sites(indexer: &Indexer, item: &CallHierarchyItem) -> Vec<Range> {
    let Some(doc) = indexer.live_doc_or_parse(&item.uri) else {
        return Vec::new();
    };
    let (Some(start), Some(end)) = (
        point_of(&doc.bytes, item.range.start),
        point_of(&doc.bytes, item.range.end),
    ) else {
        return Vec::new();
    };
    let Some(declaration) = doc.tree.root_node().descendant_for_point_range(start, end) else {
        return Vec::new();
    };
    let starts = line_starts(&doc.bytes);
    let mut sites = Vec::new();
    let mut stack = vec![declaration];
    while let Some(node) = stack.pop() {
        if let Some(name) = callee_name(node) {
            sites.push(Range::new(
                ts_pos_to_lsp(name.start_position(), &starts, &doc.bytes),
                ts_pos_to_lsp(name.end_position(), &starts, &doc.bytes),
            ));
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    sites.sort_by_key(|range| range.start);
    sites
}

/// The name node a call node calls, if `node` is a call.
pub(crate) fn callee_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "method_invocation" => node.child_by_field_name("name"),
        "object_creation_expression" => {
            let created = node.child_by_field_name("type")?;
            let mut name = created;
            while let Some(inner) = name.child_by_field_name("name").or_else(|| {
                (name.kind() == "generic_type")
                    .then(|| name.named_child(0))
                    .flatten()
            }) {
                name = inner;
            }
            Some(name)
        }
        "call_expression" => {
            let callee = node.named_child(0)?;
            match callee.kind() {
                "simple_identifier" => Some(callee),
                "navigation_expression" => {
                    let suffix = callee.named_child(
                        u32::try_from(callee.named_child_count().checked_sub(1)?).ok()?,
                    )?;
                    let mut cursor = suffix.walk();
                    let identifier = suffix
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "simple_identifier");
                    identifier
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn call_item(uri: &Url, symbol: &SymbolEntry) -> CallHierarchyItem {
    CallHierarchyItem {
        name: symbol.name.clone(),
        kind: symbol.kind,
        tags: None,
        detail: symbol.container.clone(),
        uri: uri.clone(),
        range: symbol.range,
        selection_range: symbol.selection_range,
        data: None,
    }
}

/// The innermost function around `range`, or else the innermost type (a
/// field initializer's call is made from its class).
fn enclosing_caller(symbols: &[SymbolEntry], range: Range) -> Option<&SymbolEntry> {
    let innermost = |accept: fn(SymbolKind) -> bool| {
        symbols
            .iter()
            .filter(|symbol| accept(symbol.kind) && range_contains(symbol.range, range))
            .max_by_key(|symbol| symbol.range.start)
    };
    innermost(is_callable).or_else(|| innermost(is_class_kind))
}

fn is_callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::METHOD | SymbolKind::FUNCTION | SymbolKind::CONSTRUCTOR
    )
}

/// Whether `position` lies in a comment.
fn in_comment(doc: &LiveDoc, position: Position) -> bool {
    let Some(point) = point_of(&doc.bytes, position) else {
        return false;
    };
    let mut node = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point);
    while let Some(current) = node {
        if current.kind().contains("comment") {
            return true;
        }
        node = current.parent();
    }
    false
}

#[cfg(test)]
#[path = "call_hierarchy_tests.rs"]
mod tests;

//! Java "mark occurrences" that are not a name's uses, as jdtls highlights
//! them (Eclipse's occurrence finders):
//!
//! - on `return` or a method's return type, the method's exit points: its
//!   return type, each `throw` and each `return` of its own body;
//! - on a type of a `throws` clause, the `throw`s of that exception;
//! - on a type a class `implements`, the methods implementing it;
//! - on a labelled `break`/`continue`, the label and the end of the
//!   statement it leaves.
//!
//! Lambdas and classes declared inside a method are not part of it: their
//! `return`s and `throw`s exit something else.

use tower_lsp::lsp_types::{Location, Position, SymbolKind, Url};
use tree_sitter::Node;

use crate::indexer::Indexer;
use crate::types::CursorPos;

/// The places to mark for the cursor at `position`, or `None` when the
/// cursor is on none of the constructs this module marks.
pub(crate) fn java_marked_occurrences(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<Vec<Location>> {
    if !super::java_cst::is_java_file(uri) {
        return None;
    }
    let doc = indexer.live_doc_or_parse(uri)?;
    let node = super::cst_lambda::cursor_node_at(&doc, CursorPos::from(position))?;
    let text = std::str::from_utf8(&doc.bytes).ok()?;
    let marked = marked_nodes(indexer, uri, node, &doc.bytes)?;
    Some(
        marked
            .into_iter()
            .filter_map(|marked| node_location(uri, marked, text))
            .collect(),
    )
}

fn marked_nodes<'tree>(
    indexer: &Indexer,
    uri: &Url,
    node: Node<'tree>,
    bytes: &[u8],
) -> Option<Vec<Node<'tree>>> {
    if let Some(method) = exit_point_method(node) {
        return Some(exit_points(method));
    }
    if let Some((method, thrown)) = thrown_type_of_method(node, bytes) {
        return Some(throws_of(
            method,
            node_type_name(node, bytes)?,
            thrown,
            bytes,
        ));
    }
    if let Some(class) = implementing_class(node) {
        return implementing_methods(indexer, uri, node, class, bytes);
    }
    if let Some(target) = super::java_cst::jump_target(node, bytes) {
        if let super::java_cst::JumpTarget::Label(label) = target {
            let statement = label.parent()?;
            let closing = statement
                .child_by_field_name("body")
                .or_else(|| statement.named_child(1))
                .and_then(last_token)?;
            return Some(vec![label, closing]);
        }
    }
    None
}

/// The method whose exit points the cursor asks for: on a `return`, the
/// method it returns from; on the type a method declares it returns, that
/// method.
fn exit_point_method(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "return" {
        let statement = node
            .parent()
            .filter(|parent| parent.kind() == "return_statement")?;
        return enclosing_body_owner(statement)
            .filter(|owner| owner.kind() == "method_declaration");
    }
    let mut ancestor = node;
    while let Some(parent) = ancestor.parent() {
        if parent.kind() == "method_declaration" {
            let is_the_return_type = parent
                .child_by_field_name("type")
                .is_some_and(|return_type| return_type.id() == ancestor.id());
            return is_the_return_type.then_some(parent);
        }
        if !is_type_part(parent.kind()) {
            return None;
        }
        ancestor = parent;
    }
    None
}

fn is_type_part(kind: &str) -> bool {
    matches!(
        kind,
        "generic_type" | "scoped_type_identifier" | "type_arguments" | "array_type" | "dimensions"
    )
}

/// The method, lambda or initializer whose body `node` is in.
fn enclosing_body_owner(node: Node<'_>) -> Option<Node<'_>> {
    std::iter::successors(node.parent(), |current| current.parent()).find(|current| {
        matches!(
            current.kind(),
            "method_declaration"
                | "constructor_declaration"
                | "lambda_expression"
                | "class_body"
                | "static_initializer"
        )
    })
}

/// The return type, `throw`s and `return`s of `method`'s own body.
fn exit_points(method: Node<'_>) -> Vec<Node<'_>> {
    let mut points: Vec<Node<'_>> = method.child_by_field_name("type").into_iter().collect();
    let Some(body) = method.child_by_field_name("body") else {
        return points;
    };
    for statement in own_statements(body) {
        if matches!(statement.kind(), "throw_statement" | "return_statement") {
            if let Some(keyword) = statement.child(0) {
                points.push(keyword);
            }
        }
    }
    points
}

/// Every node under `body` that is not inside a lambda or a class declared
/// there.
fn own_statements(body: Node<'_>) -> Vec<Node<'_>> {
    let mut found = Vec::new();
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        found.push(node);
        if matches!(node.kind(), "lambda_expression" | "class_body") {
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    found.sort_by_key(|node| node.start_byte());
    found
}

/// When `node` names a type in a method's `throws` clause: the method and
/// the clause's type node.
fn thrown_type_of_method<'tree>(
    node: Node<'tree>,
    bytes: &[u8],
) -> Option<(Node<'tree>, Node<'tree>)> {
    let mut thrown = node;
    while let Some(parent) = thrown.parent() {
        if parent.kind() == "throws" {
            let method = parent.parent()?;
            node_type_name(node, bytes)?;
            return Some((method, thrown));
        }
        if !is_type_part(parent.kind()) {
            return None;
        }
        thrown = parent;
    }
    None
}

/// `node`'s text when it is a type name.
fn node_type_name<'a>(node: Node<'_>, bytes: &'a [u8]) -> Option<&'a str> {
    if node.kind() != "type_identifier" {
        return None;
    }
    node.utf8_text(bytes).ok()
}

/// The thrown type `thrown` and the `throw`s in `method`'s own body that
/// throw a new `name`.
fn throws_of<'tree>(
    method: Node<'tree>,
    name: &str,
    thrown: Node<'tree>,
    bytes: &[u8],
) -> Vec<Node<'tree>> {
    let mut marked = vec![thrown];
    let Some(body) = method.child_by_field_name("body") else {
        return marked;
    };
    for statement in own_statements(body) {
        if statement.kind() != "throw_statement" {
            continue;
        }
        let throws_a_new_one = statement
            .named_child(0)
            .filter(|thrown_value| thrown_value.kind() == "object_creation_expression")
            .and_then(|creation| creation.child_by_field_name("type"))
            .and_then(|created| created.utf8_text(bytes).ok())
            .is_some_and(|created| created.rsplit('.').next() == Some(name));
        if throws_a_new_one {
            if let Some(keyword) = statement.child(0) {
                marked.push(keyword);
            }
        }
    }
    marked
}

/// The class declaration whose `implements` clause `node` names a type in.
fn implementing_class(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() != "type_identifier" {
        return None;
    }
    let mut ancestor = node;
    while let Some(parent) = ancestor.parent() {
        if parent.kind() == "super_interfaces" {
            return parent.parent().filter(|class| {
                matches!(
                    class.kind(),
                    "class_declaration" | "enum_declaration" | "record_declaration"
                )
            });
        }
        if !(is_type_part(parent.kind()) || parent.kind() == "type_list") {
            return None;
        }
        ancestor = parent;
    }
    None
}

/// The interface name `interface_name` and the names of the methods of
/// `class` that implement one of that interface's methods.
fn implementing_methods<'tree>(
    indexer: &Indexer,
    uri: &Url,
    interface_name: Node<'tree>,
    class: Node<'tree>,
    bytes: &[u8],
) -> Option<Vec<Node<'tree>>> {
    let name = interface_name.utf8_text(bytes).ok()?;
    let declared = crate::resolver::resolve_type_name(indexer, name, uri);
    let interface_methods: Vec<String> = declared
        .iter()
        .flat_map(|location| indexer.file_symbols(&location.uri))
        .filter(|symbol| {
            symbol.kind == SymbolKind::METHOD && symbol.container.as_deref() == Some(name)
        })
        .map(|symbol| symbol.name)
        .collect();
    let mut marked = vec![interface_name];
    let body = class.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        let Some(method_name) = member.child_by_field_name("name") else {
            continue;
        };
        let implements_one = method_name
            .utf8_text(bytes)
            .is_ok_and(|method| interface_methods.iter().any(|declared| declared == method));
        if implements_one {
            marked.push(method_name);
        }
    }
    Some(marked)
}

/// The last token of `node` (the `}` of a block).
fn last_token(node: Node<'_>) -> Option<Node<'_>> {
    let mut last = node;
    while last.child_count() > 0 {
        last = last.child(last.child_count() as u32 - 1)?;
    }
    Some(last)
}

fn node_location(uri: &Url, node: Node<'_>, text: &str) -> Option<Location> {
    let start = node.start_position();
    let end = node.end_position();
    let line_of = |row: usize| text.lines().nth(row);
    let start_character =
        crate::features::text_utils::utf16_column(line_of(start.row)?.get(..start.column)?);
    let end_character =
        crate::features::text_utils::utf16_column(line_of(end.row)?.get(..end.column)?);
    Some(Location::new(
        uri.clone(),
        tower_lsp::lsp_types::Range::new(
            Position::new(start.row as u32, start_character),
            Position::new(end.row as u32, end_character),
        ),
    ))
}

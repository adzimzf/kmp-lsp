//! Type hierarchy: `textDocument/prepareTypeHierarchy`,
//! `typeHierarchy/supertypes` and `typeHierarchy/subtypes`.
//!
//! A type's supertypes are the ones its declaration names (`FileData::supers`)
//! plus, in Java, the implicit one: `Object` for a class without `extends`,
//! `Enum` for an enum, `Record` for a record, `Annotation` for an annotation
//! type. Its subtypes are its direct subtypes from the subtype index: declared
//! classes, Java anonymous classes (`new Supertype() {...}`), enum constants
//! with a body (`{...}`) and lambdas whose target type is the interface.
//!
//! Names follow jdtls: a nested Java type is `Outer$Inner`, and a lambda is
//! named after the type around it, `Outer$1`.

use std::sync::Arc;

use serde_json::{json, Value};
use tower_lsp::lsp_types::{Position, Range, SymbolKind, TypeHierarchyItem, Url};
use tree_sitter::Node;

use crate::features::implementation::{is_class_kind, range_contains};
use crate::features::selection_range::point_of;
use crate::indexer::{Indexer, LiveDoc};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};
use crate::types::{FileData, SymbolEntry};

/// Java declarations that declare a type, with a `name` field.
const JAVA_TYPE_DECLARATIONS: [&str; 5] = [
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "annotation_type_declaration",
];

/// The type declared, or referred to, at `position`: the declaration when
/// the cursor is on a type's name, else the type a type name there resolves to.
pub(crate) fn prepare_type_hierarchy(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<Vec<TypeHierarchyItem>> {
    if let Some(data) = indexer.file_data_for(uri.as_str()) {
        let declared = data.symbols.iter().find(|symbol| {
            is_class_kind(symbol.kind) && contains_position(symbol.selection_range, position)
        });
        if let Some(symbol) = declared {
            return Some(vec![type_item(uri, &data, symbol)]);
        }
    }
    let (word, _) = indexer.word_and_qualifier_at(uri, position)?;
    let item = indexer
        .find_definition(&word, uri)
        .into_iter()
        .find_map(|location| declared_type_item(indexer, &location.uri, location.range))?;
    Some(vec![item])
}

/// The direct supertypes of `item`.
pub(crate) fn supertypes(indexer: &Indexer, item: &TypeHierarchyItem) -> Vec<TypeHierarchyItem> {
    let names = supertype_names(indexer, item);
    names
        .iter()
        .map(|name| {
            resolve_type(indexer, name, &item.uri).unwrap_or_else(|| external_item(name, item))
        })
        .collect()
}

/// The direct subtypes of `item`.
pub(crate) fn subtypes(indexer: &Indexer, item: &TypeHierarchyItem) -> Vec<TypeHierarchyItem> {
    let Some(name) = declared_name(item) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for location in indexer.subtypes_of(&name) {
        if location.uri == item.uri && location.range == item.selection_range {
            continue;
        }
        let Some(data) = indexer.file_data_for(location.uri.as_str()) else {
            continue;
        };
        if let Some(symbol) = data
            .symbols
            .iter()
            .find(|symbol| is_class_kind(symbol.kind) && symbol.selection_range == location.range)
        {
            items.push(type_item(&location.uri, &data, symbol));
        } else if let Some(anonymous) = data.anonymous_class_at(location.range) {
            items.push(TypeHierarchyItem {
                name: format!("new {}() {{...}}", anonymous.supertype),
                kind: SymbolKind::CLASS,
                tags: None,
                detail: None,
                uri: location.uri.clone(),
                range: Range::new(anonymous.type_name.start, anonymous.body.end),
                selection_range: anonymous.type_name,
                data: Some(json!({ "supertype": name })),
            });
        }
    }
    if is_java(&item.uri) {
        match java_declaration_kind(indexer, item).as_deref() {
            Some("enum_declaration") => items.extend(enum_constant_bodies(indexer, item, &name)),
            Some("interface_declaration") => items.extend(java_lambdas_of(indexer, &name)),
            _ => {}
        }
    }
    items
}

// ─── Items ──────────────────────────────────────────────────────────────────

/// The item of `symbol`, a type declared in `data` (the file at `uri`).
fn type_item(uri: &Url, data: &FileData, symbol: &SymbolEntry) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: display_name(uri, data, symbol),
        kind: symbol.kind,
        tags: None,
        detail: data.package.clone(),
        uri: uri.clone(),
        range: symbol.range,
        selection_range: symbol.selection_range,
        data: Some(json!({ "name": symbol.name })),
    }
}

/// The item of the type declared at `selection` in `uri`, if one is.
fn declared_type_item(indexer: &Indexer, uri: &Url, selection: Range) -> Option<TypeHierarchyItem> {
    let data = indexer.file_data_for(uri.as_str())?;
    let symbol = data
        .symbols
        .iter()
        .find(|symbol| is_class_kind(symbol.kind) && symbol.selection_range == selection)?;
    Some(type_item(uri, &data, symbol))
}

/// A supertype the workspace and its libraries don't declare (`Object`
/// without JDK sources): reported where its subtype is declared.
fn external_item(name: &str, subtype: &TypeHierarchyItem) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: name.to_string(),
        kind: SymbolKind::CLASS,
        tags: None,
        detail: None,
        uri: subtype.uri.clone(),
        range: subtype.selection_range,
        selection_range: subtype.selection_range,
        data: Some(json!({ "external": name })),
    }
}

/// `Outer$Inner` for a nested Java type, as jdtls names it; the plain name
/// otherwise.
fn display_name(uri: &Url, data: &FileData, symbol: &SymbolEntry) -> String {
    if !is_java(uri) {
        return symbol.name.clone();
    }
    let mut enclosing: Vec<&SymbolEntry> = data
        .symbols
        .iter()
        .filter(|outer| {
            is_class_kind(outer.kind)
                && outer.range != symbol.range
                && range_contains(outer.range, symbol.range)
        })
        .collect();
    enclosing.sort_by_key(|outer| outer.range.start);
    enclosing
        .iter()
        .map(|outer| outer.name.as_str())
        .chain(std::iter::once(symbol.name.as_str()))
        .collect::<Vec<_>>()
        .join("$")
}

/// The type `name` names, seen from `from`.
fn resolve_type(indexer: &Indexer, name: &str, from: &Url) -> Option<TypeHierarchyItem> {
    indexer
        .find_definition(name, from)
        .into_iter()
        .find_map(|location| declared_type_item(indexer, &location.uri, location.range))
}

/// The simple name of the declared type `item` stands for; `None` for an
/// anonymous class, a lambda or a type outside the workspace.
fn declared_name(item: &TypeHierarchyItem) -> Option<String> {
    item.data
        .as_ref()?
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ─── Supertypes ─────────────────────────────────────────────────────────────

/// What `item` extends and implements, by simple name.
fn supertype_names(indexer: &Indexer, item: &TypeHierarchyItem) -> Vec<String> {
    if let Some(supertype) = item
        .data
        .as_ref()
        .and_then(|data| data.get("supertype"))
        .and_then(Value::as_str)
    {
        return vec![supertype.to_string()];
    }
    if declared_name(item).is_none() {
        return Vec::new();
    }
    let Some(data) = indexer.file_data_for(item.uri.as_str()) else {
        return Vec::new();
    };
    let line = item.selection_range.start.line;
    let mut names: Vec<String> = data
        .supers
        .iter()
        .filter(|(super_line, _, _)| *super_line == line)
        .map(|(_, name, _)| simple_name(name).to_string())
        .collect();
    if is_java(&item.uri) {
        let implicit = match java_declaration_kind(indexer, item).as_deref() {
            Some("class_declaration") if !java_class_extends(indexer, item) => Some("Object"),
            Some("enum_declaration") => Some("Enum"),
            Some("record_declaration") => Some("Record"),
            Some("annotation_type_declaration") => Some("Annotation"),
            _ => None,
        };
        names.extend(implicit.map(str::to_string));
    }
    names.dedup();
    names
}

/// `Entry` of `Map.Entry`.
fn simple_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

// ─── Java CST ───────────────────────────────────────────────────────────────

/// The Java declaration node whose name is at `item`'s selection range.
fn with_java_declaration<T>(
    indexer: &Indexer,
    item: &TypeHierarchyItem,
    f: impl FnOnce(&LiveDoc, Node<'_>) -> T,
) -> Option<T> {
    let doc = indexer.live_doc_or_parse(&item.uri)?;
    let point = point_of(&doc.bytes, item.selection_range.start)?;
    let mut node = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point);
    while let Some(current) = node {
        if JAVA_TYPE_DECLARATIONS.contains(&current.kind())
            && current
                .child_by_field_name("name")
                .is_some_and(|name| name.start_position() == point)
        {
            return Some(f(&doc, current));
        }
        node = current.parent();
    }
    None
}

/// The tree-sitter kind of `item`'s Java declaration (`class_declaration`).
fn java_declaration_kind(indexer: &Indexer, item: &TypeHierarchyItem) -> Option<String> {
    with_java_declaration(indexer, item, |_, node| node.kind().to_string())
}

/// Whether `item`'s Java class declaration has an `extends` clause.
fn java_class_extends(indexer: &Indexer, item: &TypeHierarchyItem) -> bool {
    with_java_declaration(indexer, item, |_, node| {
        node.child_by_field_name("superclass").is_some()
    })
    .unwrap_or(false)
}

/// The enum constants of `item`'s enum that have a body: each is a subtype,
/// `{...}` to jdtls.
fn enum_constant_bodies(
    indexer: &Indexer,
    item: &TypeHierarchyItem,
    enum_name: &str,
) -> Vec<TypeHierarchyItem> {
    with_java_declaration(indexer, item, |doc, declaration| {
        let starts = line_starts(&doc.bytes);
        let Some(body) = declaration.child_by_field_name("body") else {
            return Vec::new();
        };
        let mut cursor = body.walk();
        let constants: Vec<Node<'_>> = body
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "enum_constant")
            .collect();
        constants
            .into_iter()
            .filter_map(|constant| {
                let class_body = constant.child_by_field_name("body")?;
                let name = constant.child_by_field_name("name")?;
                Some(TypeHierarchyItem {
                    name: "{...}".to_string(),
                    kind: SymbolKind::CLASS,
                    tags: None,
                    detail: None,
                    uri: item.uri.clone(),
                    range: lsp_range(&starts, doc, class_body),
                    selection_range: lsp_range(&starts, doc, name),
                    data: Some(json!({ "supertype": enum_name })),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Every lambda in the workspace's Java files whose target type is the
/// interface `name`: cast to it, or assigned to a variable or field of it.
fn java_lambdas_of(indexer: &Indexer, name: &str) -> Vec<TypeHierarchyItem> {
    let mut candidates: Vec<Url> = Vec::new();
    indexer.for_each_indexed_workspace_file(|uri, data: &Arc<FileData>| {
        if uri.ends_with(".java")
            && data.lines.iter().any(|line| line.contains("->"))
            && data.lines.iter().any(|line| line.contains(name))
        {
            if let Ok(url) = Url::parse(uri) {
                candidates.push(url);
            }
        }
        true
    });
    let mut items = Vec::new();
    for uri in candidates {
        let Some(doc) = indexer.live_doc_or_parse(&uri) else {
            continue;
        };
        let starts = line_starts(&doc.bytes);
        let mut stack = vec![doc.tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "lambda_expression"
                && lambda_target_type(node, &doc.bytes) == Some(name)
            {
                if let Some(enclosing) = enclosing_java_type_name(node, &doc.bytes) {
                    let parameters = node.child_by_field_name("parameters").unwrap_or(node);
                    items.push(TypeHierarchyItem {
                        name: format!("{enclosing}$1"),
                        kind: SymbolKind::CLASS,
                        tags: None,
                        detail: None,
                        uri: uri.clone(),
                        range: lsp_range(&starts, &doc, node),
                        selection_range: lsp_range(&starts, &doc, parameters),
                        data: Some(json!({ "supertype": name })),
                    });
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
    items
}

/// The simple name of the type `lambda` is cast or assigned to.
fn lambda_target_type<'src>(lambda: Node<'_>, bytes: &'src [u8]) -> Option<&'src str> {
    let parent = lambda.parent()?;
    let type_node = match parent.kind() {
        "cast_expression" => parent.child_by_field_name("type")?,
        "variable_declarator" => parent.parent()?.child_by_field_name("type")?,
        _ => return None,
    };
    let text = type_node.utf8_text(bytes).ok()?;
    let raw = text.split('<').next()?.trim();
    Some(simple_name(raw))
}

/// `Outer$Inner` of the named Java types around `node`.
fn enclosing_java_type_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut names = Vec::new();
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if JAVA_TYPE_DECLARATIONS.contains(&ancestor.kind()) {
            if let Some(name) = ancestor.child_by_field_name("name") {
                names.push(name.utf8_text(bytes).ok()?.to_string());
            }
        }
        current = ancestor.parent();
    }
    if names.is_empty() {
        return None;
    }
    names.reverse();
    Some(names.join("$"))
}

fn lsp_range(starts: &[usize], doc: &LiveDoc, node: Node<'_>) -> Range {
    Range::new(
        ts_pos_to_lsp(node.start_position(), starts, &doc.bytes),
        ts_pos_to_lsp(node.end_position(), starts, &doc.bytes),
    )
}

fn contains_position(range: Range, position: Position) -> bool {
    range.start <= position && position <= range.end
}

fn is_java(uri: &Url) -> bool {
    uri.path().ends_with(".java")
}

#[cfg(test)]
#[path = "type_hierarchy_tests.rs"]
mod tests;

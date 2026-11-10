//! Phase 2 for Java: tokens on references — what each name used in the code
//! (not declared there) refers to, as jdtls colors it.
//!
//! - a called name (`a.run()`, `run()`, `A::run`) is a method;
//! - a type name is its declaration's kind: class, interface, enum, an
//!   annotation type (decorator) or a type parameter in scope;
//! - `new A()` and a constructor's own name are the class;
//! - a field read (`a.count`, a bare `count` no local binds) is a property,
//!   or an enum member when it names an enum constant.
//!
//! Locals and parameters get no token here, and a name that resolves to
//! nothing is left uncolored rather than guessed (a type name is the
//! exception: it's a class unless its declaration says otherwise).

use tower_lsp::lsp_types::{SemanticTokenType, SymbolKind, Url};
use tree_sitter::Node;

use crate::indexer::{java_local_at, Indexer, LiveDoc};
use crate::queries::{
    KIND_CTOR_DECL, KIND_IDENTIFIER, KIND_OBJECT_CREATION, KIND_SCOPED_TYPE_IDENT, KIND_TYPE_IDENT,
    KIND_TYPE_PARAM, KIND_TYPE_PARAMS,
};

use super::helpers::{node_text, push_token};
use super::resolve::{resolve_symbol_kind, ResolvedReference};
use super::{type_index, RawToken, Source};

const METHOD_INVOCATION: &str = "method_invocation";
const METHOD_REFERENCE: &str = "method_reference";
const FIELD_ACCESS: &str = "field_access";
const CLASS_LITERAL: &str = "class_literal";

pub(super) fn walk_java_references(
    doc: &LiveDoc,
    src: &Source<'_>,
    indexer: &Indexer,
    uri: &Url,
    out: &mut Vec<RawToken>,
) {
    let resolver = JavaNames { src, indexer, uri };
    for node in crate::indexer::walk::descendants(doc.tree.root_node()) {
        let token_type = match node.kind() {
            KIND_TYPE_IDENT => resolver.type_identifier(node),
            KIND_IDENTIFIER => resolver.identifier(node),
            _ => None,
        };
        if let Some(token_type) = token_type {
            push_token(node, token_type, 0, src, out);
        }
    }
}

struct JavaNames<'a> {
    src: &'a Source<'a>,
    indexer: &'a Indexer,
    uri: &'a Url,
}

impl JavaNames<'_> {
    /// A `type_identifier`: a type parameter in scope, else the kind of the
    /// type it names. Declared type parameters are colored by phase 1.
    fn type_identifier(&self, node: Node<'_>) -> Option<u32> {
        if node.parent()?.kind() == KIND_TYPE_PARAM {
            return None;
        }
        let name = node_text(node, self.src.bytes);
        if is_type_parameter_in_scope(node, name, self.src.bytes) {
            return Some(type_index(&SemanticTokenType::TYPE_PARAMETER));
        }
        // `Entry` in `Map.Entry` is looked up as `Map.Entry`.
        let parent = node.parent()?;
        if parent.kind() == KIND_SCOPED_TYPE_IDENT {
            let path = std::str::from_utf8(&self.src.bytes[parent.start_byte()..node.end_byte()])
                .unwrap_or(name);
            if let Some(token_type) = self.type_kind(path) {
                return Some(token_type);
            }
        }
        Some(self.type_kind(name).unwrap_or_else(class))
    }

    /// An `identifier`, by where it stands.
    fn identifier(&self, node: Node<'_>) -> Option<u32> {
        let parent = node.parent()?;
        let name = node_text(node, self.src.bytes);
        match parent.kind() {
            METHOD_INVOCATION if is_field(parent, "name", node) => Some(method()),
            METHOD_REFERENCE if parent.named_child(0) != Some(node) => Some(method()),
            KIND_CTOR_DECL if is_field(parent, "name", node) => Some(class()),
            FIELD_ACCESS if is_field(parent, "field", node) => self.member(parent, name),
            _ if is_receiver(parent, node) => self.receiver(node, name),
            _ => self.bare_value(node, name),
        }
    }

    /// The field of `access` (`receiver.name`): an enum constant of the
    /// receiver's type, a nested type (`java.sql.Date`), else a property.
    fn member(&self, access: Node<'_>, name: &str) -> Option<u32> {
        let receiver = access.child_by_field_name("object")?;
        let receiver_type = self.named_type(receiver);
        if let Some(owner) = &receiver_type {
            if self.is_enum_constant_of(owner, name) {
                return Some(type_index(&SemanticTokenType::ENUM_MEMBER));
            }
        }
        if looks_like_type_name(name) && is_type_position(access) {
            return Some(self.type_kind(name).unwrap_or_else(class));
        }
        Some(type_index(&SemanticTokenType::PROPERTY))
    }

    /// The receiver of a call or field access: a type (`TimeZone.getDefault()`),
    /// a field, or nothing for a local or a package name.
    fn receiver(&self, node: Node<'_>, name: &str) -> Option<u32> {
        if java_local_at(node, self.src.bytes).is_some() {
            return None;
        }
        if looks_like_type_name(name) {
            if let Some(token_type) = self.type_kind(name) {
                return Some(token_type);
            }
        }
        self.field_kind(name)
    }

    /// A bare name read as a value: a field (or enum constant) unless a
    /// local or parameter binds it.
    fn bare_value(&self, node: Node<'_>, name: &str) -> Option<u32> {
        if !is_value_position(node) || java_local_at(node, self.src.bytes).is_some() {
            return None;
        }
        self.field_kind(name)
    }

    /// The token for the type declaration `name` resolves to from this file.
    /// A dotted `name` (`Map.Entry`) is resolved as a nested-type path.
    fn type_kind(&self, name: &str) -> Option<u32> {
        let resolved = if name.contains('.') {
            self.nested_type(name)?
        } else {
            resolve_symbol_kind(name, self.indexer, self.uri, is_type_kind)?
        };
        Some(match resolved.kind {
            SymbolKind::INTERFACE if self.is_annotation_type(&resolved) => {
                type_index(&SemanticTokenType::DECORATOR)
            }
            SymbolKind::INTERFACE => type_index(&SemanticTokenType::INTERFACE),
            SymbolKind::ENUM => type_index(&SemanticTokenType::ENUM),
            _ => class(),
        })
    }

    /// The declaration of the nested-type path `path` (`Map.Entry`).
    fn nested_type(&self, path: &str) -> Option<ResolvedReference> {
        crate::resolver::resolve_type_path_declarations(
            self.indexer,
            path,
            self.uri,
            crate::resolver::ResolveIo::IndexOnly,
        )
        .into_iter()
        .find_map(|location| {
            let data = self.indexer.file_data_for(location.uri.as_str())?;
            let symbol = data.symbols.iter().find(|symbol| {
                symbol.selection_range == location.range && is_type_kind(symbol.kind)
            })?;
            Some(ResolvedReference {
                kind: symbol.kind,
                uri: location.uri.clone(),
                range: symbol.range,
            })
        })
    }

    /// The token for the field or enum constant `name` resolves to.
    fn field_kind(&self, name: &str) -> Option<u32> {
        let resolved = resolve_symbol_kind(name, self.indexer, self.uri, |kind| {
            matches!(
                kind,
                SymbolKind::FIELD
                    | SymbolKind::PROPERTY
                    | SymbolKind::CONSTANT
                    | SymbolKind::ENUM_MEMBER
            )
        })?;
        Some(if resolved.kind == SymbolKind::ENUM_MEMBER {
            type_index(&SemanticTokenType::ENUM_MEMBER)
        } else {
            type_index(&SemanticTokenType::PROPERTY)
        })
    }

    /// The type a receiver expression names (`JsonToken` in
    /// `JsonToken.NUMBER`), when it's a type name rather than a value.
    fn named_type(&self, receiver: Node<'_>) -> Option<ResolvedReference> {
        if receiver.kind() != KIND_IDENTIFIER || java_local_at(receiver, self.src.bytes).is_some() {
            return None;
        }
        let name = node_text(receiver, self.src.bytes);
        if !looks_like_type_name(name) {
            return None;
        }
        resolve_symbol_kind(name, self.indexer, self.uri, is_type_kind)
    }

    fn is_enum_constant_of(&self, owner: &ResolvedReference, name: &str) -> bool {
        owner.kind == SymbolKind::ENUM
            && self
                .indexer
                .file_data_for(owner.uri.as_str())
                .is_some_and(|data| {
                    data.symbols.iter().any(|symbol| {
                        symbol.kind == SymbolKind::ENUM_MEMBER
                            && symbol.name == name
                            && contains(&owner.range, &symbol.selection_range)
                    })
                })
    }

    /// An interface declared with `@interface`.
    fn is_annotation_type(&self, resolved: &ResolvedReference) -> bool {
        self.indexer
            .file_data_for(resolved.uri.as_str())
            .and_then(|data| {
                data.symbols
                    .iter()
                    .find(|symbol| symbol.range == resolved.range)
                    .map(|symbol| symbol.detail.contains("@interface"))
            })
            .unwrap_or(false)
    }
}

fn method() -> u32 {
    type_index(&SemanticTokenType::METHOD)
}

fn class() -> u32 {
    type_index(&SemanticTokenType::CLASS)
}

fn is_type_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::CLASS | SymbolKind::INTERFACE | SymbolKind::ENUM | SymbolKind::STRUCT
    )
}

fn is_field(parent: Node<'_>, field: &str, node: Node<'_>) -> bool {
    parent.child_by_field_name(field) == Some(node)
}

/// Whether `node` is the receiver (`object`) of a call or a field access.
fn is_receiver(parent: Node<'_>, node: Node<'_>) -> bool {
    matches!(parent.kind(), METHOD_INVOCATION | FIELD_ACCESS) && is_field(parent, "object", node)
}

/// Whether the field access `access` stands where a type does: before
/// `.class`, or as the receiver of a further access (`a.b.Type.CONSTANT`).
fn is_type_position(access: Node<'_>) -> bool {
    access.parent().is_some_and(|parent| {
        parent.kind() == CLASS_LITERAL
            || (matches!(parent.kind(), METHOD_INVOCATION | FIELD_ACCESS)
                && is_field(parent, "object", access))
            || parent.kind() == KIND_OBJECT_CREATION
    })
}

/// Whether a bare identifier is read as a value — not a label, a lambda's or
/// a declaration's own name, or a part of a package or import path.
fn is_value_position(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    !matches!(
        parent.kind(),
        "scoped_identifier"
            | "package_declaration"
            | "import_declaration"
            | "labeled_statement"
            | "break_statement"
            | "continue_statement"
            | "marker_annotation"
            | "annotation"
            | "element_value_pair"
            | "variable_declarator"
            | "formal_parameter"
            | "spread_parameter"
            | "catch_formal_parameter"
            | "inferred_parameters"
            | "lambda_expression"
            | "enum_constant"
            | "method_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "annotation_type_element_declaration"
            | "resource"
            | "enhanced_for_statement"
    )
}

/// `TimeZone`, `Map` — not a constant (`MAX_SIZE`) nor a value (`count`).
fn looks_like_type_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase()) && name.chars().any(|c| c.is_lowercase())
}

/// Whether a type parameter named `name` is declared by a class, method or
/// constructor enclosing `node`.
fn is_type_parameter_in_scope(node: Node<'_>, name: &str, bytes: &[u8]) -> bool {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        let declared = (0..current.named_child_count() as u32)
            .filter_map(|index| current.named_child(index))
            .filter(|child| child.kind() == KIND_TYPE_PARAMS)
            .flat_map(|parameters| {
                (0..parameters.named_child_count() as u32)
                    .filter_map(move |index| parameters.named_child(index))
            })
            .filter(|parameter| parameter.kind() == KIND_TYPE_PARAM)
            .any(|parameter| {
                (0..parameter.named_child_count() as u32)
                    .filter_map(|index| parameter.named_child(index))
                    .find(|child| {
                        child.kind() == KIND_TYPE_IDENT || child.kind() == KIND_IDENTIFIER
                    })
                    .is_some_and(|declared| node_text(declared, bytes) == name)
            });
        if declared {
            return true;
        }
        ancestor = current.parent();
    }
    false
}

fn contains(outer: &tower_lsp::lsp_types::Range, inner: &tower_lsp::lsp_types::Range) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

#[cfg(test)]
#[path = "java_references_tests.rs"]
mod tests;

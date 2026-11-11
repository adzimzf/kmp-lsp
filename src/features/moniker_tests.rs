//! Tests for the qualified names monikers carry.

use tower_lsp::lsp_types::{Location, MonikerKind, Position, Range, Url};

use super::*;
use crate::indexer::Indexer;

const SRC: &str = "package com.example;\n\
                   public class Outer {\n\
                   \x20 public void run() {}\n\
                   \x20 private static class Inner {\n\
                   \x20   int size;\n\
                   \x20 }\n\
                   }\n";

fn indexed() -> (Indexer, Url) {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/com/example/Outer.java").unwrap();
    idx.index_content(&uri, SRC);
    (idx, uri)
}

#[test]
fn a_method_is_named_by_package_type_and_name() {
    let (idx, uri) = indexed();
    let moniker = moniker_at(&idx, &uri, Position::new(2, 15)).unwrap();
    assert_eq!(moniker.scheme, "java");
    assert_eq!(moniker.identifier, "com.example.Outer#run()");
    assert_eq!(moniker.kind, Some(MonikerKind::Export));
}

#[test]
fn a_nested_type_and_its_field_carry_every_enclosing_type() {
    let (idx, uri) = indexed();
    let inner = moniker_at(&idx, &uri, Position::new(3, 24)).unwrap();
    assert_eq!(inner.identifier, "com.example.Outer.Inner");
    assert_eq!(inner.kind, Some(MonikerKind::Local));
    let field = moniker_at(&idx, &uri, Position::new(4, 9)).unwrap();
    assert_eq!(field.identifier, "com.example.Outer.Inner#size");
}

#[test]
fn a_top_level_type_from_a_definition_location() {
    let (idx, uri) = indexed();
    let location = Location::new(
        uri.clone(),
        Range::new(Position::new(1, 13), Position::new(1, 18)),
    );
    let moniker = moniker_declared_at(&idx, &location).unwrap();
    assert_eq!(moniker.identifier, "com.example.Outer");
}

#[test]
fn no_moniker_off_a_declaration_name() {
    let (idx, uri) = indexed();
    assert!(moniker_at(&idx, &uri, Position::new(0, 3)).is_none());
}

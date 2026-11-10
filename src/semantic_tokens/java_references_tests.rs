//! Tests for [`walk_java_references`]: tokens on Java names used (not
//! declared) in the code, as jdtls colors them.

use tower_lsp::lsp_types::{SemanticTokenType, Url};

use crate::indexer::Indexer;
use crate::semantic_tokens::{full_tokens, TOKEN_TYPES};
use crate::Language;

/// Indexes `files` (path, source) under `file:///t/` and returns the URI of
/// the last one.
fn index(indexer: &Indexer, files: &[(&str, &str)]) -> Url {
    let mut last = None;
    for (path, src) in files {
        let uri = Url::parse(&format!("file:///t/{path}")).unwrap();
        indexer.index_content(&uri, src);
        indexer.store_live_tree(&uri, src);
        last = Some(uri);
    }
    last.unwrap()
}

/// The token type on the `occurrence`-th (0-based) `needle` of line `line`
/// of `src`, or `None` when no token starts there.
fn token_at(
    indexer: &Indexer,
    uri: &Url,
    src: &str,
    line: u32,
    needle: &str,
    occurrence: usize,
) -> Option<SemanticTokenType> {
    let text = src.lines().nth(line as usize).unwrap();
    let column = text
        .match_indices(needle)
        .nth(occurrence)
        .unwrap_or_else(|| panic!("no {needle:?} #{occurrence} on line {line}"))
        .0 as u32;
    let doc = indexer.live_doc_or_parse(uri).unwrap();
    let tokens = full_tokens(indexer, uri, &doc, Language::Java);
    let (mut token_line, mut token_column) = (0, 0);
    for token in tokens.data {
        token_line += token.delta_line;
        token_column = if token.delta_line == 0 {
            token_column + token.delta_start
        } else {
            token.delta_start
        };
        if token_line == line && token_column == column {
            return Some(TOKEN_TYPES[token.token_type as usize].clone());
        }
    }
    None
}

/// Calls are methods; types are their declaration's kind; `new A()` and a
/// constructor's own name are the class; type parameters in scope are
/// type parameters; locals and parameters get no reference token.
#[test]
fn calls_types_constructors_and_type_parameters() {
    let indexer = Indexer::new();
    let src = "package p;\n\
               interface Shape {}\n\
               @interface Marker {}\n\
               class Box<T> implements Shape {\n\
               \x20 Box(T value) { T held = value; run(held); }\n\
               \x20 void run(Object o) { Object copy = new Box<T>(null); copy.toString(); }\n\
               \x20 Class<?> marker() { return Marker.class; }\n\
               \x20 Runnable runner() { return this::toString; }\n\
               }\n";
    let uri = index(&indexer, &[("p/Box.java", src)]);
    let at = |line, needle, occurrence| token_at(&indexer, &uri, src, line, needle, occurrence);
    assert_eq!(at(3, "Shape", 0), Some(SemanticTokenType::INTERFACE));
    assert_eq!(
        at(4, "Box", 0),
        Some(SemanticTokenType::CLASS),
        "constructor name"
    );
    assert_eq!(at(4, "T", 0), Some(SemanticTokenType::TYPE_PARAMETER));
    assert_eq!(at(4, "run", 0), Some(SemanticTokenType::METHOD));
    assert_eq!(at(4, "held", 1), None, "a local is not a reference token");
    assert_eq!(at(5, "Box", 0), Some(SemanticTokenType::CLASS), "`new Box`");
    assert_eq!(at(5, "toString", 0), Some(SemanticTokenType::METHOD));
    assert_eq!(at(6, "Marker", 0), Some(SemanticTokenType::DECORATOR));
    assert_eq!(
        at(7, "toString", 0),
        Some(SemanticTokenType::METHOD),
        "`this::toString`"
    );
}

/// A field read is a property — through a receiver, `this`, or bare when no
/// local shadows it — and an enum constant read through its enum is an enum
/// member. A type used as a receiver is its kind.
#[test]
fn fields_enum_constants_and_type_receivers() {
    let indexer = Indexer::new();
    let kind = "package p;\npublic enum Kind { ONE, TWO }\n";
    let src = "package p;\n\
               class Node {\n\
               \x20 static final int LIMIT = 3;\n\
               \x20 Node next;\n\
               \x20 int size;\n\
               \x20 void f(Node other, int size) {\n\
               \x20   this.size = other.next.size + LIMIT + size;\n\
               \x20   Kind kind = Kind.ONE;\n\
               \x20 }\n\
               }\n";
    let uri = index(&indexer, &[("p/Kind.java", kind), ("p/Node.java", src)]);
    let at = |line, needle, occurrence| token_at(&indexer, &uri, src, line, needle, occurrence);
    assert_eq!(
        at(6, "size", 0),
        Some(SemanticTokenType::PROPERTY),
        "`this.size`"
    );
    assert_eq!(at(6, "next", 0), Some(SemanticTokenType::PROPERTY));
    assert_eq!(
        at(6, "size", 1),
        Some(SemanticTokenType::PROPERTY),
        "`next.size`"
    );
    assert_eq!(at(6, "LIMIT", 0), Some(SemanticTokenType::PROPERTY));
    assert_eq!(at(6, "size", 2), None, "the parameter shadows the field");
    assert_eq!(at(6, "other", 0), None, "a parameter used as a receiver");
    assert_eq!(
        at(7, "Kind", 1),
        Some(SemanticTokenType::ENUM),
        "a type receiver"
    );
    assert_eq!(at(7, "ONE", 0), Some(SemanticTokenType::ENUM_MEMBER));
}

/// `Entry` in `Map.Entry` is looked up as the nested type `Map.Entry`, not
/// as whatever top-level `Entry` the name alone finds.
#[test]
fn a_nested_type_is_resolved_through_its_outer_type() {
    let indexer = Indexer::new();
    let map = "package p;\npublic interface Map {\n  interface Entry {}\n}\n";
    let entry = "package p;\npublic class Entry {}\n";
    let src = "package p;\nclass A {\n  Map.Entry entry;\n}\n";
    let uri = index(
        &indexer,
        &[
            ("p/Map.java", map),
            ("p/Entry.java", entry),
            ("p/A.java", src),
        ],
    );
    let at = |line, needle, occurrence| token_at(&indexer, &uri, src, line, needle, occurrence);
    assert_eq!(at(2, "Map", 0), Some(SemanticTokenType::INTERFACE));
    assert_eq!(at(2, "Entry", 0), Some(SemanticTokenType::INTERFACE));
}

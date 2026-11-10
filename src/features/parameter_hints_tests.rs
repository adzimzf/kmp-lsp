//! Tests for [`java_parameter_hints`]: Java parameter-name hints on literal
//! arguments, resolved through go-to-definition's overload selection.

use tower_lsp::lsp_types::{InlayHintLabel, Position, Range, Url};

use super::*;
use crate::indexer::Indexer;

fn index(idx: &Indexer, path: &str, src: &str) -> Url {
    let uri = Url::parse(&format!("file:///t/{path}")).unwrap();
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    uri
}

fn whole_file(src: &str) -> Range {
    Range::new(
        Position::new(0, 0),
        Position::new(src.lines().count() as u32, 0),
    )
}

/// `(line, character, label)` of every parameter hint in `src`.
async fn hints(idx: &Indexer, uri: &Url, src: &str) -> Vec<(u32, u32, String)> {
    let mut found: Vec<(u32, u32, String)> = java_parameter_hints(idx, uri, whole_file(src))
        .await
        .into_iter()
        .map(|hint| {
            let label = match hint.label {
                InlayHintLabel::String(label) => label,
                InlayHintLabel::LabelParts(parts) => {
                    parts.into_iter().map(|part| part.value).collect()
                }
            };
            (hint.position.line, hint.position.character, label)
        })
        .collect();
    found.sort();
    found
}

fn column(src: &str, line: usize, needle: &str) -> u32 {
    src.lines().nth(line).unwrap().find(needle).unwrap() as u32
}

/// Like jdtls: literals get a hint, a variable doesn't; the overload the
/// arguments pick names the parameters — `add(int index, E element)` for two
/// arguments, `add(E e)` for one, even for a `String` passed as a type
/// variable `E` (which a closed class named `E` would not accept).
#[tokio::test]
async fn literal_arguments_are_named_by_the_overload_they_pick() {
    let idx = Indexer::new();
    index(
        &idx,
        "Box.java",
        "public class Box<E> {\n\
         \x20 public boolean add(E e) { return true; }\n\
         \x20 public void add(int index, E element) {}\n\
         }\n",
    );
    let src = "class A {\n\
               \x20 void f(Box<String> box, String text) {\n\
               \x20   box.add(\"x\");\n\
               \x20   box.add(0, null);\n\
               \x20   box.add(text);\n\
               \x20 }\n\
               }\n";
    let uri = index(&idx, "A.java", src);
    assert_eq!(
        hints(&idx, &uri, src).await,
        vec![
            (2, column(src, 2, "\"x\""), "e:".to_owned()),
            (3, column(src, 3, "0,"), "index:".to_owned()),
            (3, column(src, 3, "null"), "element:".to_owned()),
        ]
    );
}

/// `new Bag(1L, 1)` names the constructor its arguments fit, not the first
/// one declared; `this(..)` inside a class names its own constructor.
#[tokio::test]
async fn constructor_calls_are_named_by_the_constructor_they_pick() {
    let idx = Indexer::new();
    let bag = "package com.example;\n\
               public class Bag {\n\
               \x20 public Bag() {}\n\
               \x20 public Bag(long longValue, int intValue) {}\n\
               \x20 public Bag(String name) { this(1L, 2); }\n\
               }\n";
    let bag_uri = index(&idx, "com/example/Bag.java", bag);
    let src = "package com.example;\n\
               class A {\n\
               \x20 Bag bag = new Bag(1L, 1);\n\
               \x20 Bag named = new Bag(\"n\");\n\
               }\n";
    let uri = index(&idx, "com/example/A.java", src);
    assert_eq!(
        hints(&idx, &uri, src).await,
        vec![
            (2, column(src, 2, "1L"), "longValue:".to_owned()),
            (2, column(src, 2, "1)"), "intValue:".to_owned()),
            (3, column(src, 3, "\"n\""), "name:".to_owned()),
        ]
    );
    assert_eq!(
        hints(&idx, &bag_uri, bag).await,
        vec![
            (4, column(bag, 4, "1L"), "longValue:".to_owned()),
            (4, column(bag, 4, "2)"), "intValue:".to_owned()),
        ]
    );
}

/// A class literal is a `java.lang.reflect.Type`: `toJson(true, Boolean.class)`
/// is `toJson(Object src, Type typeOfSrc)`, not `toJson(Object, Appendable)`.
#[tokio::test]
async fn a_class_literal_picks_the_overload_taking_a_type() {
    let idx = Indexer::new();
    index(
        &idx,
        "Gson.java",
        "public class Gson {\n\
         \x20 public String toJson(Object src, Type typeOfSrc) { return null; }\n\
         \x20 public void toJson(Object src, Appendable writer) {}\n\
         }\n",
    );
    let src = "class A {\n\
               \x20 void f(Gson gson) { gson.toJson(true, Boolean.class); }\n\
               }\n";
    let uri = index(&idx, "A.java", src);
    assert_eq!(
        hints(&idx, &uri, src).await,
        vec![
            (1, column(src, 1, "true"), "src:".to_owned()),
            (1, column(src, 1, "Boolean.class"), "typeOfSrc:".to_owned()),
        ]
    );
}

/// A varargs parameter is named once, at its first argument; a call the
/// declaration can't take (wrong argument count) gets no hint at all.
#[tokio::test]
async fn varargs_are_named_once_and_mismatched_calls_not_at_all() {
    let idx = Indexer::new();
    index(
        &idx,
        "Out.java",
        "public class Out {\n\
         \x20 public void printf(String format, Object... args) {}\n\
         \x20 public void only(int count) {}\n\
         }\n",
    );
    let src = "class A {\n\
               \x20 void f(Out out) {\n\
               \x20   out.printf(\"%d %d\", 1, 2);\n\
               \x20   out.only(1, 2);\n\
               \x20 }\n\
               }\n";
    let uri = index(&idx, "A.java", src);
    assert_eq!(
        hints(&idx, &uri, src).await,
        vec![
            (2, column(src, 2, "\"%d"), "format:".to_owned()),
            (2, column(src, 2, "1,"), "args:".to_owned()),
        ]
    );
}

/// Kotlin files get no parameter-name hints from this provider.
#[tokio::test]
async fn kotlin_files_get_no_parameter_hints() {
    let idx = Indexer::new();
    let src = "fun f(count: Int) {}\nfun g() { f(1) }\n";
    let uri = index(&idx, "A.kt", src);
    assert!(hints(&idx, &uri, src).await.is_empty());
}

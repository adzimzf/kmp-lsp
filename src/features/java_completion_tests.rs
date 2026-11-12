//! Tests for [`java_member_completions`]: Java member completion after
//! `receiver.`, filtered by access and ranked as jdtls ranks it.

use tower_lsp::lsp_types::{Position, Url};

use super::*;
use crate::indexer::Indexer;

fn index(idx: &Indexer, path: &str, src: &str) -> Url {
    let uri = Url::parse(&format!("file:///t/{path}")).unwrap();
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    uri
}

/// The labels offered at the `|` in `src` (which is removed), best first.
async fn labels(idx: &Indexer, path: &str, src: &str) -> Vec<String> {
    let line = src.lines().position(|line| line.contains('|')).unwrap();
    let character = src.lines().nth(line).unwrap().find('|').unwrap() as u32;
    let uri = index(idx, path, &src.replacen('|', "", 1));
    let position = Position::new(line as u32, character);
    let expected = expected_argument_types(idx, &uri, position).await;
    java_member_completions(idx, &uri, position, false, expected)
        .expect("a Java member completion")
        .into_iter()
        .map(|item| item.label)
        .collect()
}

const BOX: &str = "package a;\n\
    public class Box {\n\
    \x20 private int secret;\n\
    \x20 int shared;\n\
    \x20 protected int inherited;\n\
    \x20 public String name;\n\
    \x20 public static final int LIMIT = 3;\n\
    \x20 public static Box create() { return new Box(); }\n\
    \x20 public int size() { return 0; }\n\
    \x20 public static class Inner {}\n\
    }\n";

#[tokio::test]
async fn private_members_are_offered_in_their_own_file_only() {
    let idx = Indexer::new();
    let inside = labels(
        &idx,
        "a/Box.java",
        &BOX.replace(
            "public int size() { return 0; }",
            "public int size() { return this.|; }",
        ),
    )
    .await;
    assert!(inside.contains(&"secret".to_string()), "{inside:?}");

    index(&idx, "a/Box.java", BOX);
    let same_package = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { int use(Box box) { return box.|; } }\n",
    )
    .await;
    assert!(
        !same_package.contains(&"secret".to_string()),
        "{same_package:?}"
    );
    assert!(
        same_package.contains(&"shared".to_string()),
        "{same_package:?}"
    );
    assert!(
        same_package.contains(&"inherited".to_string()),
        "{same_package:?}"
    );

    let other_package = labels(
        &idx,
        "b/User.java",
        "package b;\nimport a.Box;\nclass User { int use(Box box) { return box.|; } }\n",
    )
    .await;
    for hidden in ["secret", "shared", "inherited"] {
        assert!(
            !other_package.contains(&hidden.to_string()),
            "{other_package:?}"
        );
    }
    assert!(
        other_package.contains(&"size".to_string()),
        "{other_package:?}"
    );
}

/// After a type name, only what's static: fields, methods and nested types.
#[tokio::test]
async fn a_type_name_offers_its_static_members() {
    let idx = Indexer::new();
    index(&idx, "a/Box.java", BOX);
    let found = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use() { Object o = Box.|; } }\n",
    )
    .await;
    for offered in ["LIMIT", "create", "Inner"] {
        assert!(found.contains(&offered.to_string()), "{found:?}");
    }
    for instance in ["size", "name"] {
        assert!(!found.contains(&instance.to_string()), "{found:?}");
    }
}

/// A member of the type the context expects comes first: the variable's
/// declared type here, then the parameter type of the call's argument.
#[tokio::test]
async fn members_of_the_expected_type_come_first() {
    let idx = Indexer::new();
    index(&idx, "a/Box.java", BOX);
    let declared = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use(Box box) { String s = box.|; } }\n",
    )
    .await;
    assert_eq!(
        declared.first().map(String::as_str),
        Some("name"),
        "{declared:?}"
    );

    let argument = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User {\n\
         \x20 static void count(int n) {}\n\
         \x20 void use(Box box) { count(box.|()); }\n\
         }\n",
    )
    .await;
    assert_eq!(
        argument.first().map(String::as_str),
        Some("size"),
        "{argument:?}"
    );
}

/// With a `(` already after the cursor, methods come before fields.
#[tokio::test]
async fn a_parenthesis_after_the_cursor_ranks_methods_first() {
    let idx = Indexer::new();
    index(&idx, "a/Box.java", BOX);
    let found = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use(Box box) { box.|(); } }\n",
    )
    .await;
    assert_eq!(found.first().map(String::as_str), Some("size"), "{found:?}");
    let fields = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use(Box box) { box.| = null; } }\n",
    )
    .await;
    assert_eq!(
        fields.first().map(String::as_str),
        Some("inherited"),
        "{fields:?}"
    );
}

/// The receiver's class is the one its type names where the type was
/// written, and the overload the arguments pick decides that type:
/// `that(String)` returns `p.StringSubject`, not the other `StringSubject`.
#[tokio::test]
async fn the_receiver_class_is_resolved_where_its_type_was_written() {
    let idx = Indexer::new();
    index(
        &idx,
        "p/Builder.java",
        "package p;\npublic class Builder {\n\
         \x20 public StringSubject that(String actual) { return null; }\n\
         \x20 public Subject that(Object actual) { return null; }\n\
         }\n",
    );
    index(
        &idx,
        "p/Subject.java",
        "package p;\npublic class Subject { public void isEqualTo(Object o) {} }\n",
    );
    index(
        &idx,
        "p/StringSubject.java",
        "package p;\npublic class StringSubject extends Subject { public void contains(String s) {} }\n",
    );
    index(
        &idx,
        "q/StringSubject.java",
        "package q;\npublic class StringSubject { public void unrelated() {} }\n",
    );
    let found = labels(
        &idx,
        "t/Test.java",
        "package t;\nimport p.Builder;\nclass Test { void use(Builder b) { b.that(\"x\").|(); } }\n",
    )
    .await;
    assert!(found.contains(&"contains".to_string()), "{found:?}");
    assert!(found.contains(&"isEqualTo".to_string()), "{found:?}");
    assert!(!found.contains(&"unrelated".to_string()), "{found:?}");
}

#[test]
fn types_compare_by_simple_name_with_primitives_boxed() {
    assert!(same_type("int", "Integer"));
    assert!(same_type("java.util.List<String>", "List<T>"));
    assert!(!same_type("T", "T"));
    assert!(!same_type("String[]", "String"));
}

// ─── bare names ──────────────────────────────────────────────────────────────

/// The labels the whole completion pipeline offers at the `|` in `src`
/// (which is removed), best first — a bare name, no receiver.
fn bare_labels(idx: &Indexer, path: &str, src: &str) -> Vec<String> {
    let line = src.lines().position(|line| line.contains('|')).unwrap();
    let character = src.lines().nth(line).unwrap().find('|').unwrap() as u32;
    let uri = index(idx, path, &src.replacen('|', "", 1));
    let (items, _) = idx.completions(&uri, Position::new(line as u32, character), false);
    items.into_iter().map(|item| item.label).collect()
}

const ASYNC_CONFIG: &str = "package a;\n\
    import java.util.concurrent.Executor;\n\
    public class AsyncConfig extends Base {\n\
    \x20 private int poolSize = 10;\n\
    \x20 public Executor getAsyncExecutor(String prefix) {\n\
    \x20   Executor executor = makeExecutor();\n\
    \x20   for (String entry : prefix.split(\",\")) {\n\
    \x20     e|\n\
    \x20   }\n\
    \x20   Executor later = null;\n\
    \x20   return executor;\n\
    \x20 }\n\
    \x20 private Executor makeExecutor() { return null; }\n\
    }\n";

const BASE: &str = "package a;\n\
    public class Base {\n\
    \x20 protected void baseHelper() {}\n\
    \x20 private void basePrivate() {}\n\
    }\n";

#[test]
fn locals_in_scope_come_first_and_no_kotlin_is_offered() {
    let idx = Indexer::new();
    index(&idx, "a/Base.java", BASE);
    let labels = bare_labels(&idx, "a/AsyncConfig.java", ASYNC_CONFIG);
    assert_eq!(labels[..2], ["entry", "executor"], "{labels:?}");
    for kotlin in [
        "emptyList",
        "emptyArray",
        "error",
        "println",
        "pfun",
        "lateinit",
    ] {
        assert!(
            !labels.contains(&kotlin.to_string()),
            "{kotlin}: {labels:?}"
        );
    }
    assert!(labels.contains(&"else".to_string()), "{labels:?}");
}

#[test]
fn a_local_is_offered_only_where_it_is_in_scope() {
    let idx = Indexer::new();
    let labels = bare_labels(
        &idx,
        "a/Scopes.java",
        "package a;\nclass Scopes {\n  void run(int limit) {\n    { int inner = 1; }\n    l|\n    int late = 2;\n  }\n  void other(int lost) {}\n}\n",
    );
    assert!(labels.contains(&"limit".to_string()), "{labels:?}");
    for out_of_scope in ["inner", "late", "lost"] {
        assert!(
            !labels.contains(&out_of_scope.to_string()),
            "{out_of_scope}: {labels:?}"
        );
    }
}

#[test]
fn members_are_offered_as_java_reaches_them() {
    let idx = Indexer::new();
    index(&idx, "a/Base.java", BASE);
    index(
        &idx,
        "a/Neighbour.java",
        "package a;\nclass Neighbour { void baseLookalike() {} }\n",
    );
    let labels = bare_labels(
        &idx,
        "a/AsyncConfig.java",
        &ASYNC_CONFIG.replace("e|", "b|"),
    );
    assert!(labels.contains(&"baseHelper".to_string()), "{labels:?}");
    // Private to the superclass's file; a method of another class needs a receiver.
    assert!(!labels.contains(&"basePrivate".to_string()), "{labels:?}");
    assert!(!labels.contains(&"baseLookalike".to_string()), "{labels:?}");
}

#[test]
fn a_static_method_offers_only_static_members() {
    let idx = Indexer::new();
    let labels = bare_labels(
        &idx,
        "a/Counter.java",
        "package a;\nclass Counter {\n  private int countNow;\n  private static int countAll;\n  static void tick() {\n    c|\n  }\n}\n",
    );
    assert!(labels.contains(&"countAll".to_string()), "{labels:?}");
    assert!(!labels.contains(&"countNow".to_string()), "{labels:?}");
}

#[test]
fn keywords_follow_where_the_name_is_typed() {
    let idx = Indexer::new();
    let top_level = bare_labels(&idx, "a/Empty.java", "p|\n");
    assert_eq!(
        top_level[..3],
        ["package", "public", "import"],
        "{top_level:?}"
    );
    for kotlin in ["pfun", "print", "println", "repeat"] {
        assert!(
            !top_level.contains(&kotlin.to_string()),
            "{kotlin}: {top_level:?}"
        );
    }

    let class_body = bare_labels(&idx, "a/Body.java", "package a;\nclass Body {\n  p|\n}\n");
    assert!(
        class_body.contains(&"private".to_string()),
        "{class_body:?}"
    );
    assert!(!class_body.contains(&"super".to_string()), "{class_body:?}");
}

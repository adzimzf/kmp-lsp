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

/// The labels offered for a `case` label at the `|` in `src`, best first.
fn case_labels(idx: &Indexer, path: &str, src: &str) -> Option<Vec<String>> {
    let line = src.lines().position(|line| line.contains('|')).unwrap();
    let character = src.lines().nth(line).unwrap().find('|').unwrap() as u32;
    let uri = index(idx, path, &src.replacen('|', "", 1));
    java_case_label_completions(idx, &uri, Position::new(line as u32, character), false)
        .map(|items| items.into_iter().map(|item| item.label).collect())
}

const TOKEN: &str =
    "package a;\npublic enum Token { STRING, NUMBER, NULL; static final int NUL = 0; }\n";

#[test]
fn case_label_offers_the_switched_enums_constants() {
    let idx = Indexer::new();
    index(&idx, "a/Token.java", TOKEN);
    let shapes = [
        // complete code
        "  switch (token) {\n    case NU|:\n      return 1;\n    default:\n      return 0;\n  }\n}}\n",
        // the rest of the line cut
        "  switch (token) {\n    case NU|\n      return 1;\n    default:\n      return 0;\n  }\n}}\n",
        // the rest of the file cut
        "  switch (token) {\n    case NU|",
        // a second label of the same case
        "  switch (token) {\n    case STRING, NU|\n",
    ];
    for body in shapes {
        let src = format!("package a;\nclass Use {{ int of(Token token) {{\n{body}");
        let offered = case_labels(&idx, "a/Use.java", &src).unwrap_or_else(|| panic!("{src}"));
        assert!(
            offered.contains(&"NUMBER".to_string()),
            "{src}\n{offered:?}"
        );
        assert!(offered.contains(&"NULL".to_string()), "{src}\n{offered:?}");
        // the static field is no case label
        assert!(!offered.contains(&"NUL".to_string()), "{src}\n{offered:?}");
    }
}

#[test]
fn case_label_on_a_non_enum_switch_is_left_to_general_completion() {
    let idx = Indexer::new();
    index(&idx, "a/Token.java", TOKEN);
    let src = "package a;\nclass Use { int of(int n) {\n  switch (n) {\n    case LI|:\n      return 1;\n  }\n  return 0;\n}}\n";
    assert_eq!(case_labels(&idx, "a/Use.java", src), None);
    let src =
        "package a;\nclass Use { int of(Token token) {\n  int caseCount = NU|;\n  return 0;\n}}\n";
    assert_eq!(case_labels(&idx, "a/Use.java", src), None);
}

#[test]
fn case_label_text_is_recognised() {
    assert!(is_case_label(b"    case "));
    assert!(is_case_label(b"    case NU"));
    assert!(is_case_label(b"    case A, B, N"));
    assert!(is_case_label(b"case N"));
    // `case` itself is the keyword being typed
    assert!(!is_case_label(b"    case"));
    assert!(!is_case_label(b"    showcase N"));
    assert!(!is_case_label(b"    int x = N"));
    assert!(!is_case_label(b"    foo(a, N"));
}

/// Only names that start with what was typed, or whose camel-case words it
/// abbreviates, are offered — never one it merely occurs in.
#[tokio::test]
async fn members_are_filtered_by_what_was_typed() {
    let idx = Indexer::new();
    index(
        &idx,
        "a/Paths.java",
        "package a;\npublic final class Paths {\n\
         \x20 public static final char pathSeparatorChar = ':';\n\
         \x20 public static final String pathSeparator = \":\";\n\
         \x20 public static String separatorOfPath() { return \"\"; }\n\
         }\n",
    );
    let typed = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use() { char c = Paths.pathSeparatorC|; } }\n",
    )
    .await;
    assert_eq!(typed, vec!["pathSeparatorChar".to_string()]);

    let initials = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use() { Object o = Paths.pSC|; } }\n",
    )
    .await;
    assert_eq!(initials, vec!["pathSeparatorChar".to_string()]);

    let substring = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use() { Object o = Paths.Path|; } }\n",
    )
    .await;
    assert!(
        !substring.contains(&"separatorOfPath".to_string()),
        "{substring:?}"
    );
}

/// What the receiver's class declares comes before what it inherits, the
/// rest being equal.
#[tokio::test]
async fn the_receivers_own_members_come_before_inherited_ones() {
    let idx = Indexer::new();
    index(
        &idx,
        "a/Base.java",
        "package a;\npublic class Base { public String alpha() { return \"\"; } }\n",
    );
    index(
        &idx,
        "a/Derived.java",
        "package a;\npublic class Derived extends Base { public int zulu() { return 0; } }\n",
    );
    let found = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { void use(Derived derived) { derived.|; } }\n",
    )
    .await;
    let zulu = found.iter().position(|label| label == "zulu");
    let alpha = found.iter().position(|label| label == "alpha");
    assert!(zulu.is_some() && alpha.is_some(), "{found:?}");
    assert!(zulu < alpha, "{found:?}");
}

/// An array offers `length`, which no class declares, with no JDK
/// indexed; with one, `Object`'s members too.
#[tokio::test]
async fn an_array_offers_its_length_and_the_members_of_object() {
    let idx = Indexer::new();
    let found = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { int use(String[] names) { return names.leng|; } }\n",
    )
    .await;
    assert_eq!(found, vec!["length".to_string()]);

    index(
        &idx,
        "java/lang/Object.java",
        "package java.lang;\npublic class Object {\n\
         \x20 protected native Object clone();\n\
         \x20 public native int hashCode();\n\
         }\n",
    );
    let with_object = labels(
        &idx,
        "a/User.java",
        "package a;\nclass User { Object use(String[] names) { return names.|; } }\n",
    )
    .await;
    for offered in ["length", "hashCode"] {
        assert!(
            with_object.contains(&offered.to_string()),
            "{with_object:?}"
        );
    }
}

/// The labels offered for a package or a qualified type name at the `|`.
fn qualified_labels(idx: &Indexer, path: &str, src: &str) -> Vec<String> {
    let line = src.lines().position(|line| line.contains('|')).unwrap();
    let character = src.lines().nth(line).unwrap().find('|').unwrap() as u32;
    let uri = index(idx, path, &src.replacen('|', "", 1));
    java_qualified_name_completions(idx, &uri, Position::new(line as u32, character))
        .unwrap_or_default()
        .into_iter()
        .map(|item| item.label)
        .collect()
}

#[test]
fn an_import_offers_packages_then_the_types_of_the_package_named() {
    let idx = Indexer::new();
    index(
        &idx,
        "com/demo/data/Row.java",
        "package com.demo.data;\npublic class Row {}\n",
    );
    index(
        &idx,
        "com/demo/data/Table.java",
        "package com.demo.data;\npublic class Table {}\n",
    );
    let packages = qualified_labels(
        &idx,
        "com/demo/Foo.java",
        "package com.demo;\nimport com.dem|\npublic class Foo {}\n",
    );
    assert_eq!(packages, vec!["com.demo", "com.demo.data"]);

    let types = qualified_labels(
        &idx,
        "com/demo/Foo.java",
        "package com.demo;\nimport com.demo.data.Ro|\npublic class Foo {}\n",
    );
    assert_eq!(types, vec!["Row"]);

    let instantiated = qualified_labels(
        &idx,
        "com/demo/Foo.java",
        "package com.demo;\nclass Foo { Object o = new com.demo.data.Ta|; }\n",
    );
    assert_eq!(instantiated, vec!["Table"]);
}

#[test]
fn qualified_name_sites_are_recognised() {
    let site = |line| QualifiedNameSite::of(line).map(|site| (site.kind, site.dotted));
    assert_eq!(
        site("package com.dem"),
        Some((QualifiedNameKind::PackageDeclaration, "com.dem"))
    );
    assert_eq!(
        site("import static com.demo.Ro"),
        Some((QualifiedNameKind::Import, "com.demo.Ro"))
    );
    assert_eq!(
        site("    Object o = new com.demo.Ro"),
        Some((QualifiedNameKind::Instantiation, "com.demo.Ro"))
    );
    // A simple name after `new` is the general completion's.
    assert_eq!(site("    Object o = new Ro"), None);
    // `renew` is not `new`, and `importer` is not `import`.
    assert_eq!(site("    renew a.b"), None);
    assert_eq!(site("importer a.b"), None);
}

#[test]
fn keywords_start_declarations_and_statements_only() {
    assert!(starts_a_declaration_or_statement(""));
    assert!(starts_a_declaration_or_statement("    "));
    assert!(starts_a_declaration_or_statement("  int x = 0; "));
    assert!(starts_a_declaration_or_statement("  public "));
    assert!(!starts_a_declaration_or_statement("  int x = "));
    assert!(!starts_a_declaration_or_statement("  Object o = new "));
}

/// The keywords offered at the `|` in `src`.
fn keyword_labels(idx: &Indexer, src: &str) -> Vec<String> {
    let line = src.lines().position(|line| line.contains('|')).unwrap();
    let character = src.lines().nth(line).unwrap().find('|').unwrap() as u32;
    let uri = index(idx, "a/Foo.java", &src.replacen('|', "", 1));
    java_keyword_completions(idx, &uri, Position::new(line as u32, character))
        .into_iter()
        .map(|item| item.label)
        .collect()
}

#[test]
fn keywords_follow_the_scope_being_typed_in() {
    let idx = Indexer::new();
    assert_eq!(keyword_labels(&idx, "p|"), vec!["package", "public"]);
    assert_eq!(
        keyword_labels(&idx, "package a;\np|"),
        vec!["public"],
        "a second package declaration"
    );
    assert_eq!(
        keyword_labels(&idx, "package a;\npublic class Foo {\n    pub|\n}\n"),
        vec!["public"]
    );
    let in_block = keyword_labels(
        &idx,
        "package a;\nclass Foo {\n  void f() {\n    ret|\n  }\n}\n",
    );
    assert_eq!(in_block, vec!["return"]);
    assert!(keyword_labels(&idx, "package a;\nclass Foo {\n  int x = re|;\n}\n").is_empty());
}

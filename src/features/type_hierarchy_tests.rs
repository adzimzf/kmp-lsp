//! Tests for the type hierarchy: supertypes with Java's implicit ones, and
//! subtypes named the way jdtls names them.

use tower_lsp::lsp_types::{Position, TypeHierarchyItem, Url};

use super::*;
use crate::indexer::Indexer;

fn index(idx: &Indexer, path: &str, src: &str) -> Url {
    let uri = Url::parse(&format!("file:///t/{path}")).unwrap();
    idx.index_content(&uri, src);
    uri
}

fn prepared(idx: &Indexer, uri: &Url, line: u32, character: u32) -> TypeHierarchyItem {
    let items = prepare_type_hierarchy(idx, uri, Position::new(line, character)).unwrap();
    items.into_iter().next().unwrap()
}

fn names(items: Vec<TypeHierarchyItem>) -> Vec<String> {
    let mut names: Vec<String> = items.into_iter().map(|item| item.name).collect();
    names.sort();
    names.dedup();
    names
}

/// A class without `extends` has `Object` besides its interfaces; one with
/// `extends` has just what it names; an enum has `Enum`, an annotation type
/// `Annotation`, an interface nothing.
#[test]
fn java_supertypes_include_the_implicit_one() {
    let idx = Indexer::new();
    let iface = index(&idx, "Shape.java", "interface Shape {}\n");
    let base = index(
        &idx,
        "Base.java",
        "class Base implements Shape, Cloneable {}\n",
    );
    let sub = index(&idx, "Sub.java", "class Sub extends Base {}\n");
    let kind = index(&idx, "Kind.java", "enum Kind { A }\n");
    let note = index(&idx, "Note.java", "@interface Note {}\n");
    let of = |uri: &Url, character| names(supertypes(&idx, &prepared(&idx, uri, 0, character)));
    assert_eq!(of(&iface, 10), Vec::<String>::new());
    assert_eq!(of(&base, 6), ["Cloneable", "Object", "Shape"]);
    assert_eq!(of(&sub, 6), ["Base"]);
    assert_eq!(of(&kind, 5), ["Enum"]);
    assert_eq!(of(&note, 11), ["Annotation"]);
}

/// Subtypes are the direct ones: a nested class is `Outer$Inner`, an
/// anonymous class `new Shape() {...}`, a lambda `Outer$1`.
#[test]
fn java_subtypes_are_named_like_jdtls() {
    let idx = Indexer::new();
    let iface = index(&idx, "Shape.java", "interface Shape { int area(); }\n");
    index(
        &idx,
        "Uses.java",
        "class Uses {\n  static class Square implements Shape { public int area() { return 1; } }\n  \
         Shape a = new Shape() { public int area() { return 2; } };\n  \
         Object b = (Shape) () -> 3;\n}\n",
    );
    index(&idx, "Cube.java", "class Cube extends Uses.Square {}\n");
    let item = prepared(&idx, &iface, 0, 10);
    assert_eq!(
        names(subtypes(&idx, &item)),
        ["Uses$1", "Uses$Square", "new Shape() {...}"]
    );
}

/// An enum constant with a body is a subtype of its enum, `{...}`.
#[test]
fn java_enum_constant_bodies_are_subtypes() {
    let idx = Indexer::new();
    let uri = index(
        &idx,
        "Policy.java",
        "enum Policy {\n  DEFAULT,\n  STRING { int x() { return 1; } };\n  int x() { return 0; }\n}\n",
    );
    let item = prepared(&idx, &uri, 0, 5);
    assert_eq!(names(subtypes(&idx, &item)), ["{...}"]);
}

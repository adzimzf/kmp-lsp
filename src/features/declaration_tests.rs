//! Tests for Java `textDocument/declaration`.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use super::*;

fn indexed(files: &[(&str, &str)]) -> Indexer {
    let indexer = Indexer::new();
    for (path, source) in files {
        let uri = Url::parse(&format!("file:///t/{path}")).unwrap();
        indexer.index_content(&uri, source);
        indexer.store_live_tree(&uri, source);
    }
    indexer
}

const SHAPE: &str = "package p;\npublic interface Shape {\n  double area();\n}\n";
const BASE: &str = "package p;\npublic abstract class Base implements Shape {\n  public double area() { return 0; }\n  public void draw() {}\n}\n";
const SQUARE: &str = "package p;\npublic class Square extends Base {\n  @Override\n  public double area() { return 1; }\n}\n";
const USE: &str = "package p;\nclass Use {\n  void f(Square square) {\n    square.area();\n    square.draw();\n  }\n}\n";

fn only(response: GotoDefinitionResponse) -> Location {
    match response {
        GotoDefinitionResponse::Scalar(location) => location,
        other => panic!("expected one location, got {other:?}"),
    }
}

#[tokio::test]
async fn an_override_is_declared_by_the_topmost_method_it_overrides() {
    let indexer = indexed(&[
        ("p/Shape.java", SHAPE),
        ("p/Base.java", BASE),
        ("p/Square.java", SQUARE),
        ("p/Use.java", USE),
    ]);
    let uri = Url::parse("file:///t/p/Use.java").unwrap();
    let declaration = only(
        java_declaration(&indexer, &uri, Position::new(3, 12))
            .await
            .unwrap(),
    );
    assert!(
        declaration.uri.path().ends_with("Shape.java"),
        "{declaration:?}"
    );
    assert_eq!(declaration.range.start.line, 2);
}

#[tokio::test]
async fn a_method_that_overrides_nothing_is_declared_where_it_is_defined() {
    let indexer = indexed(&[
        ("p/Shape.java", SHAPE),
        ("p/Base.java", BASE),
        ("p/Square.java", SQUARE),
        ("p/Use.java", USE),
    ]);
    let uri = Url::parse("file:///t/p/Use.java").unwrap();
    let declaration = only(
        java_declaration(&indexer, &uri, Position::new(4, 12))
            .await
            .unwrap(),
    );
    assert!(
        declaration.uri.path().ends_with("Base.java"),
        "{declaration:?}"
    );
    assert_eq!(declaration.range.start.line, 3);
}

#[tokio::test]
async fn a_kotlin_file_is_left_to_go_to_definition() {
    let indexer = indexed(&[("p/A.kt", "package p\nfun f() {}\n")]);
    let uri = Url::parse("file:///t/p/A.kt").unwrap();
    assert!(java_declaration(&indexer, &uri, Position::new(1, 4))
        .await
        .is_none());
}

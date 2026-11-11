//! Tests for the call hierarchy items built from references.

use tower_lsp::lsp_types::{Location, Position, Range, Url};

use super::*;
use crate::indexer::Indexer;

fn at(line: u32, start: u32, end: u32) -> Range {
    Range::new(Position::new(line, start), Position::new(line, end))
}

/// References group by the method they occur in; a mention in a Javadoc
/// comment is not a call, and a field initializer's call comes from its
/// class.
#[test]
fn incoming_calls_group_by_caller_and_skip_comments() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/A.java").unwrap();
    let src =
        "class A {\n  /** Calls {@link #run()}. */\n  void f() {\n    run();\n    run();\n  }\n  \
               int x = run();\n  int run() { return 0; }\n}\n";
    idx.index_content(&uri, src);
    let item = prepare_call_hierarchy(&idx, &uri, Position::new(7, 7)).unwrap();
    assert_eq!(item.name, "run");
    let references = [at(1, 20, 23), at(3, 4, 7), at(4, 4, 7), at(6, 10, 13)]
        .into_iter()
        .map(|range| Location::new(uri.clone(), range))
        .collect();
    let calls = incoming_calls(&idx, &item, references);
    let summary: Vec<(String, Vec<u32>)> = calls
        .iter()
        .map(|call| {
            let lines = call
                .from_ranges
                .iter()
                .map(|range| range.start.line)
                .collect();
            (call.from.name.clone(), lines)
        })
        .collect();
    assert_eq!(
        summary,
        [("f".to_string(), vec![3, 4]), ("A".to_string(), vec![6])]
    );
}

/// The call sites of a method body: invocations and `new`.
#[test]
fn call_sites_are_the_called_names() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/A.java").unwrap();
    let src = "class A {\n  void f() {\n    g(new B());\n  }\n}\n";
    idx.index_content(&uri, src);
    let item = prepare_call_hierarchy(&idx, &uri, Position::new(1, 7)).unwrap();
    assert_eq!(call_sites(&idx, &item), [at(2, 4, 5), at(2, 10, 11)]);
}

//! Tests for [`compute_selection_ranges`]: the chain grows from the token
//! under the cursor through every enclosing syntax node.

use tower_lsp::lsp_types::{Position, Range, SelectionRange, Url};

use super::*;
use crate::indexer::Indexer;

fn chain(selection: &SelectionRange) -> Vec<Range> {
    let mut ranges = vec![selection.range];
    let mut parent = selection.parent.as_deref();
    while let Some(current) = parent {
        ranges.push(current.range);
        parent = current.parent.as_deref();
    }
    ranges
}

fn span(start_line: u32, start: u32, end_line: u32, end: u32) -> Range {
    Range::new(
        Position::new(start_line, start),
        Position::new(end_line, end),
    )
}

/// A method name in a call selects the name, then the call, the statement,
/// the block, the method, the class body, the class and the file; every step
/// contains the one before and no range repeats.
#[test]
fn java_call_name_grows_through_its_enclosing_nodes() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/A.java").unwrap();
    let src = "class A {\n  void f(A a) {\n    a.run(1);\n  }\n}\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let selections = compute_selection_ranges(&idx, &uri, &[Position::new(2, 6)]).unwrap();
    let ranges = chain(&selections[0]);
    assert_eq!(ranges[0], span(2, 6, 2, 9), "`run`");
    assert_eq!(ranges[1], span(2, 4, 2, 12), "`a.run(1)`");
    assert_eq!(ranges[2], span(2, 4, 2, 13), "`a.run(1);`");
    assert_eq!(ranges.last(), Some(&span(0, 0, 5, 0)), "the file");
    for (inner, outer) in ranges.iter().zip(ranges.iter().skip(1)) {
        assert!(inner != outer, "{inner:?} repeats");
        assert!(
            outer.start <= inner.start && inner.end <= outer.end,
            "{outer:?} doesn't contain {inner:?}"
        );
    }
}

/// Columns are UTF-16: an identifier after a non-ASCII string is selected
/// at its LSP position, and a Kotlin file works the same way.
#[test]
fn kotlin_positions_after_non_ascii_text_select_the_identifier() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/A.kt").unwrap();
    let src = "fun f() { val s = \"é\"; println(s) }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let column = src.find("println").unwrap() as u32 - 1; // `é` is 2 bytes, 1 UTF-16 unit
    let selections = compute_selection_ranges(&idx, &uri, &[Position::new(0, column)]).unwrap();
    assert_eq!(selections[0].range, span(0, column, 0, column + 7));
    assert!(chain(&selections[0]).len() >= 3);
}

/// A position on a line past the end of the file selects only itself.
#[test]
fn a_position_past_the_end_selects_itself() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/A.java").unwrap();
    let src = "class A {}\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let position = Position::new(9, 0);
    let selections = compute_selection_ranges(&idx, &uri, &[position]).unwrap();
    assert_eq!(selections[0].range, Range::new(position, position));
    assert!(selections[0].parent.is_none());
}

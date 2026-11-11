//! `textDocument/selectionRange`: expand the selection syntactically.
//!
//! From the token under the cursor (an identifier, a literal, a keyword) the
//! selection grows through every enclosing syntax node — `getAdapter`, then
//! `gson.getAdapter(type)`, the statement, the block, the method, the class,
//! the file — like jdtls does for Java. Nodes that cover exactly the same
//! text as the one inside them are one step, not two.

use tower_lsp::lsp_types::{Position, Range, SelectionRange, Url};
use tree_sitter::{Node, Point};

use crate::indexer::live_tree::utf16_col_to_byte;
use crate::indexer::{Indexer, LiveDoc};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};

/// One selection chain per position of `positions` in `uri`, innermost
/// first. `None` when the file has no parse tree (an unsupported language).
pub(crate) fn compute_selection_ranges(
    indexer: &Indexer,
    uri: &Url,
    positions: &[Position],
) -> Option<Vec<SelectionRange>> {
    let doc = indexer.live_doc_or_parse(uri)?;
    Some(
        positions
            .iter()
            .map(|position| selection_range_at(&doc, *position))
            .collect(),
    )
}

/// The selection chain at `position` in `doc`. A position past the end of
/// the text selects just itself.
pub(crate) fn selection_range_at(doc: &LiveDoc, position: Position) -> SelectionRange {
    let empty = SelectionRange {
        range: Range::new(position, position),
        parent: None,
    };
    let Some(point) = point_of(&doc.bytes, position) else {
        return empty;
    };
    let Some(innermost) = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    else {
        return empty;
    };
    let starts = line_starts(&doc.bytes);
    let range_of = |node: Node<'_>| {
        Range::new(
            ts_pos_to_lsp(node.start_position(), &starts, &doc.bytes),
            ts_pos_to_lsp(node.end_position(), &starts, &doc.bytes),
        )
    };
    // Innermost first, without repeating a range an enclosing node shares.
    let mut ranges: Vec<Range> = Vec::new();
    let mut node = Some(innermost);
    while let Some(current) = node {
        let range = range_of(current);
        if ranges.last() != Some(&range) {
            ranges.push(range);
        }
        node = current.parent();
    }
    ranges
        .into_iter()
        .rev()
        .fold(None, |parent, range| {
            Some(SelectionRange {
                range,
                parent: parent.map(Box::new),
            })
        })
        .unwrap_or(empty)
}

/// The tree-sitter point (row, byte column) of an LSP position, or `None`
/// when the line doesn't exist.
pub(crate) fn point_of(bytes: &[u8], position: Position) -> Option<Point> {
    let text = std::str::from_utf8(bytes).ok()?;
    let line = text.split('\n').nth(position.line as usize)?;
    Some(Point::new(
        position.line as usize,
        utf16_col_to_byte(line, position.character as usize),
    ))
}

#[cfg(test)]
#[path = "selection_range_tests.rs"]
mod tests;

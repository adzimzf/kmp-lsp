//! CST-based folding ranges — walks the live tree-sitter tree to find foldable regions.
//!
//! Block nodes (class body, function body, lambdas, etc.) become `Region` folds.
//! Consecutive `line_comment` sibling runs become `Comment` folds.
//! `import_list` nodes become `Imports` folds.
//! `multiline_comment` nodes spanning multiple lines become `Comment` folds.

use tower_lsp::lsp_types::{FoldingRange, FoldingRangeKind, Url};

use crate::indexer::Indexer;
use crate::queries::{
    KIND_BLOCK, KIND_BLOCK_COMMENT, KIND_CATCH_BLOCK, KIND_CLASS_BODY, KIND_CONTROL_STRUCTURE_BODY,
    KIND_ENUM_CLASS_BODY, KIND_FUN_BODY, KIND_IMPORT_LIST, KIND_INTERFACE_BODY, KIND_LAMBDA_LIT,
    KIND_LINE_COMMENT, KIND_MULTILINE_COMMENT, KIND_OBJECT_BODY,
};

/// Block node kinds that should be folded as `Region` ranges.
const REGION_KINDS: &[&str] = &[
    KIND_CLASS_BODY,
    KIND_INTERFACE_BODY,
    KIND_ENUM_CLASS_BODY,
    KIND_OBJECT_BODY,
    KIND_FUN_BODY,
    KIND_BLOCK,
    KIND_CATCH_BLOCK,
    KIND_CONTROL_STRUCTURE_BODY,
    KIND_LAMBDA_LIT,
    // Java: `interface_body`, `class_body` and `block` are shared with Kotlin.
    "constructor_body",
    "enum_body",
    "annotation_type_body",
    "switch_block",
    "switch_block_statement_group",
    "array_initializer",
];

/// Java declarations whose fold starts at their name, not at the `{` of their
/// body (as jdtls folds them): `Excluder withExclusionStrategy(` with its
/// parameters on the following lines folds from the name's line, and so does
/// a class whose `implements` clause wraps. Members of an anonymous class fold
/// from their `{`.
const JAVA_DECLARATION_KINDS: &[&str] = &[
    "method_declaration",
    "constructor_declaration",
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "annotation_type_declaration",
];

fn is_anonymous_class_member(node: tree_sitter::Node) -> bool {
    node.parent()
        .filter(|body| body.kind() == KIND_CLASS_BODY)
        .and_then(|body| body.parent())
        .is_some_and(|owner| owner.kind() == "object_creation_expression")
}

/// Compute folding ranges for `uri` using the live parse tree.
///
/// Returns `None` when no live tree exists for the file.
pub(crate) fn cst_folding_ranges(indexer: &Indexer, uri: &Url) -> Option<Vec<FoldingRange>> {
    let doc = indexer.live_doc(uri)?;
    let root = doc.tree.root_node();
    let mut ranges = Vec::new();

    collect_folds(root, &doc.bytes, &mut ranges);

    if ranges.is_empty() {
        None
    } else {
        Some(ranges)
    }
}

fn region(start: usize, end: usize) -> FoldingRange {
    FoldingRange {
        start_line: start as u32,
        end_line: end as u32,
        start_character: None,
        end_character: None,
        kind: Some(FoldingRangeKind::Region),
        collapsed_text: None,
    }
}

/// Which grammar a tree is in: Java folds the way jdtls does, which differs
/// from Kotlin in where a block's fold ends and how comments fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Java,
    Kotlin,
}

impl Syntax {
    /// Java's tree-sitter root is a `program`; Kotlin's a `source_file`.
    fn of(root: tree_sitter::Node) -> Self {
        if root.kind() == "program" {
            Self::Java
        } else {
            Self::Kotlin
        }
    }
}

/// What a fold walk keeps fixed: the source text and its grammar.
#[derive(Clone, Copy)]
struct Source<'a> {
    bytes: &'a [u8],
    syntax: Syntax,
}

fn collect_folds(node: tree_sitter::Node, bytes: &[u8], out: &mut Vec<FoldingRange>) {
    let source = Source {
        bytes,
        syntax: Syntax::of(node),
    };
    if source.syntax == Syntax::Java {
        collect_region_comment_folds(node, bytes, out);
    }
    collect_folds_at(node, out, 0, source);
}

fn collect_folds_at(
    node: tree_sitter::Node,
    out: &mut Vec<FoldingRange>,
    depth: usize,
    source: Source<'_>,
) {
    // See `crate::util::MAX_CST_DESCENT_DEPTH`: bail rather than overflow the
    // stack on a pathologically deep tree (huge chained expression, or
    // ERROR-recovery on a huge malformed file).
    if depth >= crate::util::MAX_CST_DESCENT_DEPTH {
        crate::util::report_cst_depth_exceeded!("collect_folds_at", node);
        return;
    }

    let kind = node.kind();

    if JAVA_DECLARATION_KINDS.contains(&kind) && !is_anonymous_class_member(node) {
        if let (Some(name), Some(body)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("body"),
        ) {
            let start = name.start_position().row.min(body.start_position().row);
            let end = body.end_position().row;
            if end > start {
                out.push(region(start, end));
            }
            for child in node.children(&mut node.walk()) {
                if child.id() == body.id() {
                    // The body's own fold is the declaration's: fold what is inside it.
                    collect_children_folds(body, out, depth + 1, source);
                } else {
                    collect_folds_at(child, out, depth + 1, source);
                }
            }
            return;
        }
    }

    if source.syntax == Syntax::Java && kind == "static_initializer" {
        // jdtls folds a static initializer from `static` to its `}`, even
        // when it is all on one line.
        let start = node.start_position().row;
        let end = node.end_position().row;
        out.push(region(start, end));
        if let Some(body) = node.named_child(0) {
            collect_children_folds(body, out, depth + 1, source);
        }
        return;
    }

    if source.syntax == Syntax::Java && REGION_KINDS.contains(&kind) {
        if let Some(end) = java_region_end(node) {
            out.push(region(node.start_position().row, end));
        }
    } else if REGION_KINDS.contains(&kind) {
        let start = node.start_position();
        let end = node.end_position();
        // A `{ … }` block keeps its closing line in view, so it needs a line
        // between its braces to fold; a `case` group folds from two lines.
        let closes_with_brace = node
            .child(node.child_count().saturating_sub(1) as u32)
            .is_some_and(|last| last.kind() == "}");
        let shown_lines = if closes_with_brace { 1 } else { 0 };
        if end.row > start.row + shown_lines {
            out.push(FoldingRange {
                start_line: start.row as u32,
                end_line: end.row as u32,
                start_character: None,
                end_character: None,
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    } else if kind == KIND_MULTILINE_COMMENT || kind == KIND_BLOCK_COMMENT {
        let start = node.start_position();
        let end = node.end_position();
        if end.row > start.row {
            out.push(FoldingRange {
                start_line: start.row as u32,
                end_line: end.row as u32,
                start_character: None,
                end_character: None,
                kind: Some(FoldingRangeKind::Comment),
                collapsed_text: None,
            });
        }
    } else if kind == KIND_IMPORT_LIST {
        let start = node.start_position();
        let end = node.end_position();
        if end.row > start.row {
            out.push(FoldingRange {
                start_line: start.row as u32,
                end_line: end.row as u32,
                start_character: None,
                end_character: None,
                kind: Some(FoldingRangeKind::Imports),
                collapsed_text: None,
            });
        }
    }

    collect_children_folds(node, out, depth, source);
}

/// The last line of a Java region's fold, as jdtls has it, or `None` when
/// it does not fold:
/// - a `case` group runs to the line before the next label (or the
///   switch's `}`), blank lines included;
/// - a `{ … }` block runs to its `}`, or to the line before when the `}`
///   is followed by more of the statement (`} else {`, `} catch (..) {`);
/// - a block that is all a `case` group holds folds with the group.
fn java_region_end(node: tree_sitter::Node) -> Option<usize> {
    let start = node.start_position().row;
    if node.kind() == "switch_block_statement_group" {
        let next = node.next_sibling()?;
        let end = next.start_position().row.checked_sub(1)?;
        return (end > start).then_some(end);
    }
    let in_a_case_group = node
        .parent()
        .is_some_and(|parent| parent.kind() == "switch_block_statement_group");
    if node.kind() == "block" && in_a_case_group {
        return None;
    }
    let close = node.end_position().row;
    let continued_on_the_closing_line = node
        .next_sibling()
        .is_some_and(|next| next.start_position().row == close && next.kind() != ";");
    let end = if continued_on_the_closing_line {
        close.checked_sub(1)?
    } else {
        close
    };
    (end > start).then_some(end)
}

/// Region-marker comment pairs (`// region` … `// endregion`,
/// `// #region` … `// #endregion`, `// <editor-fold>` … `// </editor-fold>`)
/// anywhere in a Java tree, each folded from its start marker to its end.
fn collect_region_comment_folds(
    root: tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<FoldingRange>,
) {
    let mut open: Vec<usize> = Vec::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut comments = Vec::new();
    while let Some(node) = stack.pop() {
        if node.kind() == KIND_LINE_COMMENT {
            comments.push(node);
            continue;
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    comments.sort_by_key(|comment| comment.start_byte());
    for comment in comments {
        let row = comment.start_position().row;
        match RegionMarker::of(comment, bytes) {
            Some(RegionMarker::Start) => open.push(row),
            Some(RegionMarker::End) => {
                if let Some(start) = open.pop() {
                    if row > start {
                        out.push(region(start, row));
                    }
                }
            }
            None => {}
        }
    }
}

/// A line comment that opens or closes a folding region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionMarker {
    Start,
    End,
}

impl RegionMarker {
    fn of(comment: tree_sitter::Node, bytes: &[u8]) -> Option<Self> {
        let text = comment.utf8_text(bytes).ok()?;
        let text = text.trim_start_matches('/').trim();
        let starts = ["region", "#region", "<editor-fold"];
        let ends = ["endregion", "#endregion", "</editor-fold"];
        let is_word_end = |rest: &str| {
            rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('>')
        };
        let marked = |markers: &[&str]| {
            markers
                .iter()
                .any(|marker| text.strip_prefix(marker).is_some_and(is_word_end))
        };
        if marked(&ends) {
            Some(Self::End)
        } else if marked(&starts) {
            Some(Self::Start)
        } else {
            None
        }
    }
}

/// Whether a Java line comment can start a run of comments that folds: one
/// on a line of its own (not trailing code) and not a region marker.
/// Anything that is not a line comment can't.
fn starts_a_java_comment_run(node: tree_sitter::Node, bytes: &[u8]) -> bool {
    if node.kind() != KIND_LINE_COMMENT {
        return true;
    }
    let trails_code = node
        .prev_sibling()
        .is_some_and(|previous| previous.end_position().row == node.start_position().row);
    !trails_code && RegionMarker::of(node, bytes).is_none()
}

/// Folds within `node`'s children: runs of line comments and of imports fold
/// as one; every other child is walked.
fn collect_children_folds(
    node: tree_sitter::Node,
    out: &mut Vec<FoldingRange>,
    depth: usize,
    source: Source<'_>,
) {
    let mut walker = node.walk();
    let children: Vec<_> = node.children(&mut walker).collect();
    let mut i = 0;
    while i < children.len() {
        let child = children[i];
        if child.kind() == "import_declaration" {
            // Java has no import list node: a run of imports folds as one.
            let run_start = child.start_position().row;
            let mut run_end = child.end_position().row;
            let mut j = i + 1;
            while j < children.len()
                && matches!(children[j].kind(), "import_declaration" | KIND_LINE_COMMENT)
            {
                run_end = children[j].end_position().row;
                j += 1;
            }
            if run_end > run_start {
                out.push(FoldingRange {
                    kind: Some(FoldingRangeKind::Imports),
                    ..region(run_start, run_end)
                });
            }
            i = j;
        } else if source.syntax == Syntax::Java && !starts_a_java_comment_run(child, source.bytes) {
            i += 1;
        } else if child.kind() == KIND_LINE_COMMENT {
            let run_start = child.start_position().row;
            let mut run_end = run_start;
            let mut j = i + 1;
            while j < children.len() && children[j].kind() == KIND_LINE_COMMENT {
                let next_row = children[j].start_position().row;
                let continues_the_run = source.syntax == Syntax::Kotlin
                    || (next_row == run_end + 1
                        && RegionMarker::of(children[j], source.bytes).is_none());
                if !continues_the_run {
                    break;
                }
                run_end = next_row;
                j += 1;
            }
            if run_end > run_start {
                out.push(FoldingRange {
                    start_line: run_start as u32,
                    end_line: run_end as u32,
                    start_character: None,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Comment),
                    collapsed_text: None,
                });
            }
            i = j;
        } else {
            collect_folds_at(child, out, depth + 1, source);
            i += 1;
        }
    }
}

#[cfg(test)]
mod depth_guard_tests {
    //! Regression coverage for the `MAX_CST_DESCENT_DEPTH` guard added to
    //! `collect_folds`. Before the guard, a long chain of `+` (perfectly
    //! valid Kotlin — no malformed/ERROR-recovery input needed) parses as a
    //! deeply left-nested `binary_expression` tree, and `collect_folds`'s
    //! unconditional full-tree recursion overflowed an 8 MiB stack (matching
    //! Linux's real main-thread default) at ~10-20k chained terms in a debug
    //! build. This test proves the guard actually stops the descent — run on
    //! a thread with that same 8 MiB stack, well past the old crash
    //! threshold, and it must both survive and terminate promptly.
    use tree_sitter::Parser;

    #[test]
    fn collect_folds_does_not_overflow_on_a_pathologically_deep_chain() {
        let n = 200_000; // ~3x the old debug-build crash threshold
        let mut src = String::with_capacity(n * 2 + 32);
        src.push_str("fun f() { val x = 1");
        for _ in 0..n {
            src.push_str("+1");
        }
        src.push_str("\n}\n");

        let handle = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024) // match Linux's default main-thread stack
            .spawn(move || {
                let mut parser = Parser::new();
                parser
                    .set_language(&tree_sitter_kotlin::LANGUAGE.into())
                    .unwrap();
                let tree = parser.parse(&src, None).unwrap();
                let mut out = Vec::new();
                super::collect_folds(tree.root_node(), src.as_bytes(), &mut out);
                out.len()
            })
            .unwrap();
        // If this doesn't overflow, `join` returns normally (a stack overflow
        // aborts the whole process rather than failing this join).
        handle.join().expect("must not overflow the stack");
    }
}

#[cfg(test)]
mod java_tests {
    use tree_sitter::Parser;

    fn fold_starts(source: &str) -> Vec<(usize, usize)> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut out = Vec::new();
        super::collect_folds(tree.root_node(), source.as_bytes(), &mut out);
        let mut starts: Vec<(usize, usize)> = out
            .iter()
            .map(|range| (range.start_line as usize, range.end_line as usize))
            .collect();
        starts.sort();
        starts
    }

    #[test]
    fn constructors_switches_and_cases_fold() {
        let source = "class A {\n  A(int x) {\n    super();\n  }\n  int f(int x) {\n    switch (x) {\n      case 1:\n        a();\n        return 1;\n      default:\n        return 0;\n    }\n  }\n}\n";
        assert_eq!(
            fold_starts(source),
            vec![(0, 13), (1, 3), (4, 12), (5, 11), (6, 8), (9, 10)]
        );
    }

    #[test]
    fn a_method_with_wrapped_parameters_folds_from_its_name() {
        let source = "class A {\n  void f(\n      int a,\n      int b) {\n    a();\n  }\n}\n";
        assert_eq!(fold_starts(source), vec![(0, 6), (1, 5)]);
    }

    #[test]
    fn a_wrapped_class_header_folds_from_its_name_an_anonymous_member_from_its_brace() {
        let source = "class A\n    implements B {\n  Object o = new B() {\n    void f(\n        int a) {\n      a();\n    }\n  };\n}\n";
        assert_eq!(fold_starts(source), vec![(0, 8), (2, 7), (4, 6)]);
    }

    #[test]
    fn enum_and_annotation_bodies_and_imports_fold() {
        let source = "import a.B;\nimport a.C;\n\nenum E {\n  ONE,\n  TWO\n}\n@interface S {\n  int value();\n  int other();\n}\n";
        assert_eq!(fold_starts(source), vec![(0, 1), (3, 6), (7, 10)]);
    }
}

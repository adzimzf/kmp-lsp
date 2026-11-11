//! Tests for Java re-indentation.

use tower_lsp::lsp_types::{FormattingOptions, Position, Range, TextEdit};

use super::*;

fn options() -> FormattingOptions {
    FormattingOptions {
        tab_size: 2,
        insert_spaces: true,
        ..Default::default()
    }
}

fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

fn apply(text: &str, edits: &[TextEdit]) -> String {
    let mut lines = lines(text);
    for edit in edits {
        let line = &mut lines[edit.range.start.line as usize];
        let end = edit.range.end.character as usize;
        *line = format!("{}{}", edit.new_text, &line[end..]);
    }
    lines.join("\n")
}

fn unindent(text: &str) -> String {
    text.split('\n')
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n")
}

const FORMATTED: &str = "package com.example;

/**
 * A class.
 */
public class Example
    extends Base {
  private static final Adapter adapter =
      new Adapter() {
        @Override
        public void write(String value) {
          out.value(value);
        }
      };

  int sum(
      int first,
      int second) {
    String text = \"{(\" + '}';
    switch (first) {
      case 1:
        return first;
      default:
        break;
    }
    return foo
        .bar(first, second); // }
  }
}";

#[test]
fn a_file_without_indentation_is_indented_again() {
    let broken = unindent(FORMATTED);
    let edits = format_document(&lines(&broken), &options());
    assert_eq!(apply(&broken, &edits), FORMATTED);
}

#[test]
fn a_formatted_file_has_nothing_to_change() {
    assert!(format_document(&lines(FORMATTED), &options()).is_empty());
}

#[test]
fn a_range_is_indented_and_nothing_else_moves() {
    let mut broken = lines(FORMATTED);
    for line in &mut broken[21..=26] {
        *line = line.trim_start().to_owned();
    }
    // Something outside the range that is off stays off.
    broken[0] = format!(" {}", broken[0]);
    let broken = broken.join("\n");
    let range = Range::new(Position::new(21, 0), Position::new(26, 5));
    let edits = format_range(&lines(&broken), range, &options());
    assert!(edits
        .iter()
        .all(|edit| (21..=26).contains(&edit.range.start.line)));
    let expected = format!(" {FORMATTED}");
    assert_eq!(apply(&broken, &edits), expected);
}

#[test]
fn a_text_block_is_left_alone() {
    let text = "class A {\nString s = \"\"\"\n  {\n\"\"\";\nint b;\n}";
    let edits = format_document(&lines(text), &options());
    assert_eq!(
        apply(text, &edits),
        "class A {\n  String s = \"\"\"\n  {\n\"\"\";\n  int b;\n}"
    );
}

#[test]
fn a_new_line_after_a_statement_is_indented_like_it() {
    let text = "class A {\n      void f() {\n        int a = 1;\n\n        int b = 2;\n      }\n}";
    let edits = indent_new_line(&lines(text), 3, &options()).unwrap();
    assert_eq!(
        apply(text, &edits),
        "class A {\n      void f() {\n        int a = 1;\n        \n        int b = 2;\n      }\n}"
    );
}

#[test]
fn a_new_line_already_indented_needs_no_edit() {
    let text = "class A {\n  void f() {\n    a();\n    \n  }\n}";
    assert!(indent_new_line(&lines(text), 3, &options()).is_none());
}

#[test]
fn arguments_and_conditions_continue_like_google_java_format() {
    let formatted = "class A {\n  void f() {\n    throw new IllegalStateException(\n        \"text \"\n            + more);\n    Field bound =\n        createField(\n            context,\n            field);\n    if (first != null\n        && second == null) {\n      g();\n    }\n  }\n}";
    assert!(format_document(&lines(formatted), &options()).is_empty());
    let broken = unindent(formatted);
    let edits = format_document(&lines(&broken), &options());
    assert_eq!(apply(&broken, &edits), formatted);
}

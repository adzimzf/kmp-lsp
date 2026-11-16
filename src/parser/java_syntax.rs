//! Java syntax errors reported where javac reports them.
//!
//! tree-sitter's error recovery is right about *that* a file is broken but
//! not always about *where*: an unclosed string swallows the rest of its
//! line and turns the whole class into one error node, a class header with
//! no body becomes an error at the `class` keyword, and `int x = ;` blames
//! the `=`. This module finds those three shapes and words them as javac
//! does; every other error node is described the generic way.

use tree_sitter::Node;

use super::ts_to_lsp;
use crate::types::SyntaxError;
use tower_lsp::lsp_types::{Position, Range};

/// A `"` string that reaches the end of its line without its closing quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnclosedString {
    /// Byte offset of the opening quote.
    quote: usize,
    /// Byte offset of the end of the quote's line.
    line_end: usize,
    /// Where the quote is, as the rest of the errors are written.
    position: Position,
}

/// What the parser should read in place of `source`: the same file with
/// every unclosed string closed at once, so that the rest of the file is
/// parsed as the code it is.
pub(super) struct Repaired {
    pub(super) text: String,
    unclosed: Vec<UnclosedString>,
}

impl Repaired {
    /// `source` with each unclosed string replaced by `""` and blanks, byte
    /// for byte, so every other position stays where it was.
    pub(super) fn new(source: &str) -> Self {
        let unclosed = unclosed_strings(source);
        let mut bytes = source.as_bytes().to_vec();
        for string in &unclosed {
            let region = &mut bytes[string.quote..string.line_end];
            region.fill(b' ');
            if region.len() >= 2 {
                region[..2].copy_from_slice(b"\"\"");
            }
        }
        let text = String::from_utf8(bytes).unwrap_or_else(|_| source.to_owned());
        Self { text, unclosed }
    }

    pub(super) fn has_unclosed_strings(&self) -> bool {
        !self.unclosed.is_empty()
    }

    /// `errors` of the repaired text, with each unclosed string reported
    /// at its quote. Whatever the parser made of the rest of that line, or
    /// of a stretch of code around the string, is the string's own fault
    /// (the code the string swallowed is gone), so it is left out.
    pub(super) fn report(&self, errors: Vec<SyntaxError>) -> Vec<SyntaxError> {
        let caused_by_a_string = |error: &SyntaxError| {
            self.unclosed.iter().any(|string| {
                let quote = string.position;
                let later_on_its_line = error.range.start.line == quote.line
                    && error.range.start.character >= quote.character;
                let around_it = error.range.start <= quote && quote < error.range.end;
                later_on_its_line || around_it
            })
        };
        let mut reported: Vec<SyntaxError> = errors
            .into_iter()
            .filter(|error| !caused_by_a_string(error))
            .collect();
        reported.extend(self.unclosed.iter().map(|string| SyntaxError {
            range: Range::new(string.position, string.position),
            message: "unclosed string literal".to_owned(),
        }));
        reported.sort_by_key(|error| (error.range.start.line, error.range.start.character));
        reported
    }
}

/// The javac reading of the error node `node`, when it is one of the shapes
/// tree-sitter reports in the wrong place; `None` leaves it to the generic
/// description.
pub(super) fn describe_error(node: Node<'_>, bytes: &[u8]) -> Option<SyntaxError> {
    type_header_without_body(node, bytes).or_else(|| initializer_without_expression(node, bytes))
}

/// `class Main` (or an interface, enum or record) with no `{` after it:
/// the body is missing at the end of the header.
fn type_header_without_body(node: Node<'_>, bytes: &[u8]) -> Option<SyntaxError> {
    let text = node.utf8_text(bytes).ok()?;
    let mut words = text
        .split_whitespace()
        .skip_while(|word| is_declaration_modifier(word));
    let keyword = words.next()?;
    let declares_a_type = matches!(keyword, "class" | "interface" | "enum" | "record");
    if !declares_a_type || text.contains('{') {
        return None;
    }
    let end = ts_to_lsp(node.range()).end;
    Some(SyntaxError {
        range: Range::new(end, end),
        message: "missing `{`".to_owned(),
    })
}

fn is_declaration_modifier(word: &str) -> bool {
    matches!(
        word,
        "public"
            | "protected"
            | "private"
            | "abstract"
            | "static"
            | "final"
            | "sealed"
            | "non-sealed"
            | "strictfp"
    )
}

/// `int x = ;`: the `=` of a declaration with nothing after it. The
/// expression is missing where the next token starts.
fn initializer_without_expression(node: Node<'_>, bytes: &[u8]) -> Option<SyntaxError> {
    let lone_equals = node.utf8_text(bytes).ok()?.trim() == "=";
    let in_a_declaration = node.parent().is_some_and(|parent| {
        matches!(
            parent.kind(),
            "variable_declarator" | "local_variable_declaration" | "field_declaration"
        )
    });
    if !(lone_equals && in_a_declaration) {
        return None;
    }
    let next = std::iter::successors(Some(node), |current| current.parent())
        .find_map(|current| current.next_sibling())?;
    let start = ts_to_lsp(next.range()).start;
    Some(SyntaxError {
        range: Range::new(start, start),
        message: "missing expression".to_owned(),
    })
}

/// Where a character of Java source is, for the string scan.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lexical {
    Code,
    LineComment,
    BlockComment,
    String { quote: usize },
    TextBlock,
    Character,
}

/// Every `"` string of `source` that its line ends inside. Comments, text
/// blocks and character literals are skipped: a quote in them opens nothing.
fn unclosed_strings(source: &str) -> Vec<UnclosedString> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut state = Lexical::Code;
    let mut line = 0u32;
    let mut line_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        let rest = &bytes[index..];
        if byte == b'\n' {
            if let Lexical::String { quote } = state {
                found.push(UnclosedString {
                    quote,
                    line_end: line_end_before_newline(bytes, index),
                    position: Position::new(line, (quote - line_start) as u32),
                });
                state = Lexical::Code;
            }
            if matches!(state, Lexical::LineComment | Lexical::Character) {
                state = Lexical::Code;
            }
            line += 1;
            line_start = index + 1;
            index += 1;
            continue;
        }
        let mut step = 1;
        state = match state {
            Lexical::Code if rest.starts_with(b"//") => Lexical::LineComment,
            Lexical::Code if rest.starts_with(b"/*") => {
                step = 2;
                Lexical::BlockComment
            }
            Lexical::Code if rest.starts_with(b"\"\"\"") => {
                step = 3;
                Lexical::TextBlock
            }
            Lexical::Code if byte == b'"' => Lexical::String { quote: index },
            Lexical::Code if byte == b'\'' => Lexical::Character,
            Lexical::BlockComment if rest.starts_with(b"*/") => {
                step = 2;
                Lexical::Code
            }
            Lexical::TextBlock if rest.starts_with(b"\"\"\"") => {
                step = 3;
                Lexical::Code
            }
            Lexical::String { .. } | Lexical::TextBlock | Lexical::Character if byte == b'\\' => {
                step = 2;
                state
            }
            Lexical::String { .. } if byte == b'"' => Lexical::Code,
            Lexical::Character if byte == b'\'' => Lexical::Code,
            unchanged => unchanged,
        };
        index += step;
    }
    if let Lexical::String { quote } = state {
        found.push(UnclosedString {
            quote,
            line_end: line_end_before_newline(bytes, bytes.len()),
            position: Position::new(line, (quote - line_start) as u32),
        });
    }
    found
}

/// `newline` itself, or the `\r` before it.
fn line_end_before_newline(bytes: &[u8], newline: usize) -> usize {
    if newline > 0 && bytes.get(newline - 1) == Some(&b'\r') {
        newline - 1
    } else {
        newline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quotes(source: &str) -> Vec<(u32, u32)> {
        unclosed_strings(source)
            .iter()
            .map(|string| (string.position.line, string.position.character))
            .collect()
    }

    #[test]
    fn a_string_closed_on_its_line_is_not_unclosed() {
        assert!(quotes("String a = \"x\\\"y\";\nchar c = '\"';\n").is_empty());
    }

    #[test]
    fn a_string_its_line_ends_inside_is_unclosed_at_its_quote() {
        assert_eq!(quotes("int a;\n  return \"open; }\n}\n"), vec![(1, 9)]);
    }

    #[test]
    fn quotes_in_comments_and_text_blocks_open_nothing() {
        let source = "// a \" quote\n/* \" */\nString t = \"\"\"\n  \"one\n  \"\"\";\n";
        assert!(quotes(source).is_empty());
    }

    #[test]
    fn the_repaired_text_keeps_every_other_byte_in_place() {
        let source = "a(\"open\n)\n";
        let repaired = Repaired::new(source);
        assert_eq!(repaired.text, "a(\"\"   \n)\n");
        assert!(repaired.has_unclosed_strings());
    }
}

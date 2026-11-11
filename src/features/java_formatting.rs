//! Java indentation: `textDocument/formatting`, `textDocument/rangeFormatting`
//! and the indentation of a new line typed after a statement.
//!
//! Only leading whitespace is ever changed, so a format never changes the
//! code. A line's indentation follows from the brackets that are open where it
//! starts:
//!
//! - inside `{ … }` a line is one step deeper than the line that opened the
//!   block; inside `( … )` or `[ … ]`, two steps (a continuation);
//! - a line that starts by closing a bracket lines up with the line that
//!   opened it;
//! - a statement that goes on past the end of its line (the line does not end
//!   with `;`, `{`, `}`, `,` or `:`, and is not an annotation) continues two
//!   steps deeper, and so does an argument inside `( … )` that goes on past
//!   the end of its line (`"text"` with `+ more` on the next);
//! - the statements under a `case …:` label are one step deeper than it;
//! - a block comment's ` * ` lines sit one column right of its `/*`.
//!
//! Strings, character literals, text blocks and comments are skipped when
//! counting brackets, and the inside of a text block is never touched.

use tower_lsp::lsp_types::{FormattingOptions, Position, Range, TextEdit};

/// Edits that re-indent every line of a Java file.
pub(crate) fn format_document(lines: &[String], options: &FormattingOptions) -> Vec<TextEdit> {
    let wanted = indentation(lines, step(options), |_| false);
    edits_for(lines, &wanted, options, 0..lines.len(), false)
}

/// Edits that re-indent the lines `range` covers, leaving the rest of the file
/// as it is (the lines before it set the context).
pub(crate) fn format_range(
    lines: &[String],
    range: Range,
    options: &FormattingOptions,
) -> Vec<TextEdit> {
    let first = range.start.line as usize;
    let mut last = range.end.line as usize;
    // A range ending at the start of a line does not cover that line.
    if range.end.character == 0 && last > first {
        last -= 1;
    }
    let last = last.min(lines.len().saturating_sub(1));
    let wanted = indentation(lines, step(options), |line| line < first || line > last);
    edits_for(lines, &wanted, options, first..last + 1, false)
}

/// The edit that indents the (blank) line `line` the way a statement there
/// would be indented, every other line staying as written; `None` when it
/// already is.
pub(crate) fn indent_new_line(
    lines: &[String],
    line: usize,
    options: &FormattingOptions,
) -> Option<Vec<TextEdit>> {
    let text = lines.get(line)?;
    if !text.trim().is_empty() {
        return None;
    }
    let wanted = indentation(lines, step(options), |other| other != line);
    let edits = edits_for(lines, &wanted, options, line..line + 1, true);
    (!edits.is_empty()).then_some(edits)
}

fn step(options: &FormattingOptions) -> usize {
    (options.tab_size as usize).max(1)
}

fn edits_for(
    lines: &[String],
    wanted: &[Option<usize>],
    options: &FormattingOptions,
    covered: std::ops::Range<usize>,
    fill_blank: bool,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    for line in covered {
        let (Some(text), Some(Some(width))) = (lines.get(line), wanted.get(line)) else {
            continue;
        };
        // A format leaves blank lines blank; only a new line typed in is filled.
        if text.trim().is_empty() && !fill_blank {
            continue;
        }
        let current = leading_whitespace(text);
        let replacement = whitespace(*width, options);
        if current != replacement {
            edits.push(TextEdit {
                range: Range::new(
                    Position::new(line as u32, 0),
                    Position::new(line as u32, current.len() as u32),
                ),
                new_text: replacement,
            });
        }
    }
    edits
}

fn whitespace(width: usize, options: &FormattingOptions) -> String {
    if options.insert_spaces {
        " ".repeat(width)
    } else {
        let tab = step(options);
        format!("{}{}", "\t".repeat(width / tab), " ".repeat(width % tab))
    }
}

fn leading_whitespace(text: &str) -> &str {
    &text[..text.len() - text.trim_start_matches([' ', '\t']).len()]
}

/// The width of `text`'s leading whitespace, a tab counting as `step`.
fn width_of(text: &str, step: usize) -> usize {
    leading_whitespace(text)
        .chars()
        .map(|character| if character == '\t' { step } else { 1 })
        .sum()
}

/// An open bracket.
struct Frame {
    /// The bracket: `{`, `(` or `[`.
    bracket: char,
    /// The indentation of the line that opened it: where its closer goes.
    base: usize,
    /// The indentation of the lines inside it.
    inner: usize,
    /// The last line inside the bracket left its statement (in a block) or
    /// its argument (in parentheses) unfinished.
    continued: bool,
    /// The lines inside a `{` block are under a `case …:` label.
    under_label: bool,
    /// The indentation of the line the statement now going on started at.
    statement_start: usize,
}

/// Where the scan is at the end of a line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    /// Inside a `/* … */` comment opened at this indentation.
    BlockComment(usize),
    TextBlock,
}

/// The indentation each line should have (`None`: leave it, a blank line or
/// the inside of a text block). Lines for which `keep` holds keep the
/// indentation they have, and the brackets they open are measured from it.
fn indentation(lines: &[String], step: usize, keep: impl Fn(usize) -> bool) -> Vec<Option<usize>> {
    let mut frames = vec![Frame {
        bracket: '{',
        base: 0,
        inner: 0,
        continued: false,
        under_label: false,
        statement_start: 0,
    }];
    let mut mode = Mode::Code;
    let mut wanted = Vec::with_capacity(lines.len());
    for (number, text) in lines.iter().enumerate() {
        let trimmed = text.trim();
        match mode {
            Mode::TextBlock => {
                wanted.push(None);
                let (next_mode, last_code) = scan(text, Mode::TextBlock, &mut Vec::new());
                mode = next_mode;
                if mode == Mode::Code {
                    let top = frames
                        .last_mut()
                        .expect("the file's own frame is never popped");
                    top.continued = !matches!(last_code, Some(';' | ',' | '{' | '}'));
                }
                continue;
            }
            Mode::BlockComment(opener) => {
                wanted.push(if trimmed.is_empty() {
                    None
                } else if trimmed.starts_with('*') {
                    Some(opener + 1)
                } else {
                    Some(width_of(text, step))
                });
                mode = scan(text, mode, &mut Vec::new()).0;
                continue;
            }
            Mode::Code => {}
        }
        if trimmed.is_empty() {
            // A blank line is where a new statement would go.
            let top = frames.last().expect("the file's own frame is never popped");
            wanted.push(Some(top.inner + label_indent(top, step)));
            continue;
        }

        let top = frames.last().expect("the file's own frame is never popped");
        let starts_with_closer = trimmed.starts_with(['}', ')', ']']);
        let is_label = is_case_label(trimmed);
        let computed = if starts_with_closer {
            top.base
        } else if top.continued {
            top.inner + label_indent(top, step) + 2 * step
        } else if is_label {
            top.inner
        } else {
            top.inner + label_indent(top, step)
        };
        let is_comment_line = trimmed.starts_with("//") || trimmed.starts_with("/*");
        wanted.push(Some(computed));
        let indent = if keep(number) {
            width_of(text, step)
        } else {
            computed
        };

        let top = frames
            .last_mut()
            .expect("the file's own frame is never popped");
        let is_continuation = top.bracket == '{' && top.continued && !starts_with_closer;
        if !is_continuation {
            top.statement_start = indent;
        }
        // An anonymous class or lambda body belongs to its own line.
        let header_continues =
            is_continuation && !trimmed.contains("new ") && !trimmed.contains("->");
        let header_base = top.statement_start;
        let mut line_base = indent;

        let mut events = Vec::new();
        let (next_mode, last_code) = scan(text, Mode::Code, &mut events);
        // Brackets: openers push, closers pop.
        let mut pushed_here = 0;
        let depth_before = frames.len();
        for bracket in events {
            match bracket {
                '{' | '(' | '[' => {
                    let (depth, base) = if bracket == '{' {
                        // A declaration's body (`extends Base {` on its own
                        // line) belongs to the declaration.
                        let base = if header_continues {
                            line_base.min(header_base)
                        } else {
                            line_base
                        };
                        (step, base)
                    } else {
                        (2 * step, line_base)
                    };
                    frames.push(Frame {
                        bracket,
                        base,
                        inner: base + depth,
                        continued: false,
                        under_label: false,
                        statement_start: base + depth,
                    });
                    pushed_here += 1;
                }
                _ => {
                    if frames.len() > 1 {
                        let closed = frames.pop().expect("checked above");
                        if pushed_here > 0 {
                            pushed_here -= 1;
                        } else {
                            // `int b) {` closes a bracket an earlier line opened:
                            // a block opened after it belongs to that line.
                            line_base = line_base.min(closed.base);
                        }
                    }
                }
            }
        }
        if let Mode::BlockComment(_) = next_mode {
            // The comment's own lines line up with this one.
            mode = Mode::BlockComment(indent);
        } else {
            mode = next_mode;
        }
        if is_comment_line && last_code.is_none() {
            continue;
        }
        let opened_here = frames.len() > depth_before;
        let top = frames
            .last_mut()
            .expect("the file's own frame is never popped");
        if is_label {
            top.under_label = trimmed.ends_with(':');
        }
        top.continued = if top.bracket == '{' {
            match last_code {
                Some(';' | '{' | '}' | ',' | ':') | None => false,
                Some(_) => !is_annotation(trimmed),
            }
        } else {
            // Inside `( … )` an argument ends at its `,`; `"a"` followed by
            // `+ b` on the next line continues it. What follows `if (a` on
            // its own line is the parentheses' first line, not a continuation.
            !opened_here && !matches!(last_code, Some(',' | '(' | '[' | '{') | None)
        };
    }
    wanted
}

fn label_indent(frame: &Frame, step: usize) -> usize {
    if frame.under_label {
        step
    } else {
        0
    }
}

fn is_case_label(trimmed: &str) -> bool {
    trimmed.starts_with("case ") || trimmed.starts_with("default:") || trimmed == "default"
}

/// `@Override`, `@SuppressWarnings("unchecked")`: an annotation alone on its line.
fn is_annotation(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix('@') else {
        return false;
    };
    let name_end = rest
        .find(|character: char| {
            !(character.is_alphanumeric() || character == '_' || character == '.')
        })
        .unwrap_or(rest.len());
    let after = rest[name_end..].trim();
    after.is_empty() || (after.starts_with('(') && after.ends_with(')'))
}

/// Scans one line from `mode`: pushes its brackets (outside strings and
/// comments) into `brackets`, and returns the mode at the end of the line and
/// the last code character on it.
fn scan(text: &str, mut mode: Mode, brackets: &mut Vec<char>) -> (Mode, Option<char>) {
    let characters: Vec<char> = text.chars().collect();
    let mut last_code = None;
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        let next = characters.get(index + 1).copied();
        match mode {
            Mode::BlockComment(_) => {
                if character == '*' && next == Some('/') {
                    mode = Mode::Code;
                    index += 1;
                }
            }
            Mode::TextBlock => {
                if character == '\\' {
                    index += 1;
                } else if text_block_quotes(&characters, index) {
                    mode = Mode::Code;
                    last_code = Some('"');
                    index += 2;
                }
            }
            Mode::Code => match character {
                '/' if next == Some('/') => break,
                '/' if next == Some('*') => {
                    mode = Mode::BlockComment(0);
                    index += 1;
                }
                '"' if text_block_quotes(&characters, index) => {
                    mode = Mode::TextBlock;
                    index += 2;
                }
                '"' | '\'' => {
                    index = literal_end(&characters, index);
                    last_code = Some(character);
                }
                '{' | '(' | '[' | '}' | ')' | ']' => {
                    brackets.push(character);
                    last_code = Some(character);
                }
                _ if character.is_whitespace() => {}
                _ => last_code = Some(character),
            },
        }
        index += 1;
    }
    (mode, last_code)
}

fn text_block_quotes(characters: &[char], index: usize) -> bool {
    characters.get(index..index + 3) == Some(&['"', '"', '"'])
}

/// The index of the quote closing the literal that opens at `start`.
fn literal_end(characters: &[char], start: usize) -> usize {
    let quote = characters[start];
    let mut index = start + 1;
    while index < characters.len() {
        match characters[index] {
            '\\' => index += 1,
            character if character == quote => return index,
            _ => {}
        }
        index += 1;
    }
    characters.len()
}

#[cfg(test)]
#[path = "java_formatting_tests.rs"]
mod tests;

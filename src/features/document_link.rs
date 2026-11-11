//! `textDocument/documentLink`: every `http://` / `https://` URL written in a
//! comment (Java, Kotlin or Swift: `//` and `/* … */`) links to itself.
//! URLs inside string literals are code, not links.

use tower_lsp::lsp_types::{DocumentLink, Position, Range, Url};

use super::text_utils::utf16_column;

/// The links in a file's comments.
pub(crate) fn document_links(lines: &[String]) -> Vec<DocumentLink> {
    let mut links = Vec::new();
    let mut in_block_comment = false;
    for (number, line) in lines.iter().enumerate() {
        for (start, end) in comment_spans(line, &mut in_block_comment) {
            for (url_start, url) in urls_in(&line[start..end]) {
                let column = start + url_start;
                let first = utf16_column(&line[..column]);
                links.push(DocumentLink {
                    range: Range::new(
                        Position::new(number as u32, first),
                        Position::new(number as u32, first + utf16_column(url)),
                    ),
                    target: Url::parse(url).ok(),
                    tooltip: None,
                    data: None,
                });
            }
        }
    }
    links
}

/// The byte spans of `line` that are comment text, given whether the line
/// starts inside a block comment (updated for the next line).
fn comment_spans(line: &str, in_block_comment: &mut bool) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    let mut comment_start = if *in_block_comment { Some(0) } else { None };
    while index < bytes.len() {
        if let Some(start) = comment_start {
            if bytes[index..].starts_with(b"*/") {
                spans.push((start, index));
                comment_start = None;
                *in_block_comment = false;
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                spans.push((index + 2, bytes.len()));
                return spans;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                comment_start = Some(index + 2);
                *in_block_comment = true;
                index += 2;
            }
            quote @ (b'"' | b'\'') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    if let Some(start) = comment_start {
        spans.push((start, bytes.len()));
    }
    spans
}

/// `(byte offset, url)` of each http(s) URL in `text`, trailing punctuation
/// that ends the sentence around it left out.
fn urls_in(text: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find("http") {
        let start = from + offset;
        let rest = &text[start..];
        let scheme = ["https://", "http://"]
            .into_iter()
            .find(|scheme| rest.starts_with(scheme));
        let Some(scheme) = scheme else {
            from = start + 4;
            continue;
        };
        let length = rest
            .find(|character: char| character.is_whitespace() || "<>\"'(){}[]".contains(character))
            .unwrap_or(rest.len());
        let url = rest[..length].trim_end_matches(['.', ',', ';', ':', '*']);
        if url.len() > scheme.len() {
            found.push((start, url));
        }
        from = start + length.max(scheme.len());
    }
    found
}

#[cfg(test)]
#[path = "document_link_tests.rs"]
mod tests;

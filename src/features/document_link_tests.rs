//! Tests for links in comments.

use super::*;

fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

fn found(text: &str) -> Vec<(u32, u32, u32, String)> {
    document_links(&lines(text))
        .into_iter()
        .map(|link| {
            (
                link.range.start.line,
                link.range.start.character,
                link.range.end.character,
                link.target.map(|url| url.to_string()).unwrap_or_default(),
            )
        })
        .collect()
}

#[test]
fn a_url_in_a_javadoc_line_is_a_link() {
    let text = "/*\n * See http://www.apache.org/licenses/LICENSE-2.0.\n */";
    assert_eq!(
        found(text),
        vec![(
            1,
            7,
            49,
            "http://www.apache.org/licenses/LICENSE-2.0".into()
        )]
    );
}

#[test]
fn a_url_in_an_html_attribute_and_a_line_comment() {
    let text =
        "/** <a href=\"https://example.com/a\">x</a> */\nint a; // https://example.org/b, then";
    assert_eq!(
        found(text),
        vec![
            (0, 13, 34, "https://example.com/a".into()),
            (1, 10, 31, "https://example.org/b".into()),
        ]
    );
}

#[test]
fn a_url_in_a_string_is_not_a_link() {
    assert!(found("String s = \"https://example.com\"; // not http:// alone").is_empty());
}

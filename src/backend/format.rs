//! Hover Markdown formatting helpers.
//!
//! These functions turn a [`ResolvedSymbol`] into the final Markdown string
//! returned by the `textDocument/hover` handler.  They are presentation-only
//! and contain no resolution logic.

use crate::indexer::lookup::{lang_str, symbol_kw_for_lang};
use crate::indexer::resolution::ResolvedSymbol;

/// Format a standard symbol hover: fenced code block + optional KDoc block.
///
/// The signature comes first, as jdtls and most servers show it: a long doc
/// comment must not push the name it documents out of view.
///
/// ```text
/// ```kotlin
/// fun foo(x: Int): String
/// ```
///
/// ---
///
/// KDoc comment
/// ```
pub(crate) fn format_symbol_hover(info: &ResolvedSymbol, uri_path: &str) -> String {
    let lang = lang_str(uri_path);
    let sig = info.signature.as_str();

    let java_line;
    let sig = if lang == "java" && !sig.is_empty() {
        java_line = java_declaration_line(sig, info.kind);
        java_line.as_str()
    } else {
        sig
    };
    let code_block = if sig.is_empty() {
        // Signature unavailable — fall back to keyword + known symbol name.
        format!(
            "```{lang}\n{} {}\n```",
            symbol_kw_for_lang(info.kind, lang),
            info.name
        )
    } else {
        format!("```{lang}\n{sig}\n```")
    };

    // Body assembled from the widened record: an optional deprecation marker
    // above the signature and an optional `package.Container` context footer for
    // members — both read straight off `ResolvedSymbol`, no second index lookup.
    let mut body = String::new();
    if info.deprecated {
        body.push_str("**⚠ Deprecated**\n\n");
    }
    body.push_str(&code_block);
    if let Some(ctx) = qualified_context(info) {
        body.push_str(&format!("\n\n*in `{ctx}`*"));
    }

    if info.doc.is_empty() {
        body
    } else {
        format!("{body}\n\n---\n\n{}", info.doc)
    }
}

/// The `package.Container` home of a *member* symbol, for the hover footer.
///
/// Returns `None` for top-level declarations (no enclosing container) so their
/// hover stays unchanged; `package` is folded in when the file declares one.
fn qualified_context(info: &ResolvedSymbol) -> Option<String> {
    let container = info.container.as_deref()?;
    match info.package.as_deref() {
        Some(pkg) => Some(format!("{pkg}.{container}")),
        None => Some(container.to_string()),
    }
}

/// Format a contextual hover for an `it` / named lambda parameter:
///
/// ```text
/// ```kotlin
/// val it: AccountType
/// ```
///
/// ---
///
/// <optional type-symbol hover>
/// ```
///
/// `type_sig_md` — the synthesized declaration line, e.g. `"val it: AccountType"`.
/// `type_detail` — optional hover markdown for the resolved type symbol itself.
pub(crate) fn format_contextual_hover(
    type_sig_md: &str,
    uri_path: &str,
    type_detail: Option<&str>,
) -> String {
    let lang = lang_str(uri_path);
    let sig_block = format!("```{lang}\n{type_sig_md}\n```");
    match type_detail {
        Some(td) if !td.is_empty() => format!("{sig_block}\n\n---\n\n{td}"),
        _ => sig_block,
    }
}

/// Java modifiers jdtls leaves out of a hover's declaration line.
const JAVA_MODIFIERS: &[&str] = &[
    "public",
    "protected",
    "private",
    "static",
    "final",
    "abstract",
    "default",
    "synchronized",
    "native",
    "transient",
    "volatile",
    "strictfp",
    "sealed",
    "non-sealed",
];

/// Keywords that open a Java type declaration.
const JAVA_TYPE_KEYWORDS: &[&str] = &["class", "interface", "enum", "record", "@interface"];

/// A Java declaration's source text as jdtls writes it in a hover: no
/// annotations, modifiers, body or trailing `;`/`,`; a type as its name and
/// type parameters (`Box<T>`, not `class Box<T> extends …`); an enum
/// constant as its name; a field's initializer only for a constant
/// (`final`), whose value it is.
pub(crate) fn java_declaration_line(
    signature: &str,
    kind: tower_lsp::lsp_types::SymbolKind,
) -> String {
    let collapsed = signature.split_whitespace().collect::<Vec<_>>().join(" ");
    let without_body = match collapsed.find('{') {
        Some(brace) => collapsed[..brace].to_owned(),
        None => collapsed,
    };
    let trimmed = without_body
        .trim_end_matches(|character: char| {
            matches!(character, ';' | ',') || character.is_whitespace()
        })
        .to_owned();
    let (is_final, rest) = without_java_modifiers(&trimmed);
    if kind == tower_lsp::lsp_types::SymbolKind::ENUM_MEMBER {
        return identifier_at_start(rest).to_owned();
    }
    let type_keyword = JAVA_TYPE_KEYWORDS.iter().find(|keyword| {
        rest.strip_prefix(**keyword)
            .is_some_and(|after| after.starts_with(' '))
    });
    if let Some(keyword) = type_keyword {
        let after_keyword = rest[keyword.len()..].trim_start();
        return type_name_with_parameters(after_keyword).to_owned();
    }
    let is_field = matches!(
        kind,
        tower_lsp::lsp_types::SymbolKind::FIELD
            | tower_lsp::lsp_types::SymbolKind::CONSTANT
            | tower_lsp::lsp_types::SymbolKind::PROPERTY
            | tower_lsp::lsp_types::SymbolKind::VARIABLE
    );
    if is_field && !is_final {
        if let Some(equals) = rest.find(" =") {
            return rest[..equals].to_owned();
        }
    }
    rest.to_owned()
}

/// `line` without its leading annotations and modifiers, and whether one of
/// them was `final`.
fn without_java_modifiers(line: &str) -> (bool, &str) {
    let mut rest = line.trim_start();
    let mut is_final = false;
    loop {
        if let Some(annotation) = rest.strip_prefix('@') {
            if annotation.starts_with("interface ") {
                return (is_final, rest);
            }
            let name_end = annotation
                .find(|character: char| {
                    !(character.is_alphanumeric() || matches!(character, '_' | '.'))
                })
                .unwrap_or(annotation.len());
            let mut after = &annotation[name_end..];
            if after.starts_with('(') {
                after = after.get(balanced_end(after, '(', ')')..).unwrap_or("");
            }
            rest = after.trim_start();
            continue;
        }
        let word = rest.split(' ').next().unwrap_or("");
        if JAVA_MODIFIERS.contains(&word) && rest.len() > word.len() {
            is_final |= word == "final";
            rest = rest[word.len()..].trim_start();
            continue;
        }
        return (is_final, rest);
    }
}

/// The byte just past the bracket closing the one `text` starts with.
fn balanced_end(text: &str, open: char, close: char) -> usize {
    let mut depth = 0usize;
    for (index, character) in text.char_indices() {
        if character == open {
            depth += 1;
        } else if character == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return index + character.len_utf8();
            }
        }
    }
    text.len()
}

fn identifier_at_start(text: &str) -> &str {
    let end = text
        .find(|character: char| !(character.is_alphanumeric() || matches!(character, '_' | '$')))
        .unwrap_or(text.len());
    &text[..end]
}

/// `Box<T>` of `Box<T> extends Base<T>`.
fn type_name_with_parameters(text: &str) -> &str {
    let name = identifier_at_start(text);
    let after = &text[name.len()..];
    if after.starts_with('<') {
        return &text[..name.len() + balanced_end(after, '<', '>')];
    }
    name
}

#[cfg(test)]
mod java_declaration_line_tests {
    use super::java_declaration_line;
    use tower_lsp::lsp_types::SymbolKind;

    #[test]
    fn a_type_is_its_name_and_type_parameters() {
        assert_eq!(
            java_declaration_line(
                "public class Box<T extends A<B>> extends C {",
                SymbolKind::CLASS
            ),
            "Box<T extends A<B>>"
        );
        assert_eq!(
            java_declaration_line("interface Inner", SymbolKind::INTERFACE),
            "Inner"
        );
        assert_eq!(
            java_declaration_line("@Deprecated record R(int value) {", SymbolKind::STRUCT),
            "R"
        );
    }

    #[test]
    fn a_member_loses_its_modifiers_and_semicolon() {
        assert_eq!(
            java_declaration_line("static void foo2(String s, String s2)", SymbolKind::METHOD),
            "void foo2(String s, String s2)"
        );
        assert_eq!(
            java_declaration_line("double area();", SymbolKind::METHOD),
            "double area()"
        );
        assert_eq!(
            java_declaration_line("@Override public String label() {", SymbolKind::METHOD),
            "String label()"
        );
        assert_eq!(
            java_declaration_line("private double radius = 1.0;", SymbolKind::FIELD),
            "double radius"
        );
    }

    #[test]
    fn a_constant_keeps_its_value() {
        assert_eq!(
            java_declaration_line("static final int LIMIT = 10;", SymbolKind::CONSTANT),
            "int LIMIT = 10"
        );
        assert_eq!(
            java_declaration_line(
                "public static final String NAME = \"demo\";",
                SymbolKind::FIELD
            ),
            "String NAME = \"demo\""
        );
    }

    #[test]
    fn an_enum_constant_is_its_name() {
        assert_eq!(
            java_declaration_line("METRE,", SymbolKind::ENUM_MEMBER),
            "METRE"
        );
        assert_eq!(
            java_declaration_line("FOOT(0.3048),", SymbolKind::ENUM_MEMBER),
            "FOOT"
        );
    }
}

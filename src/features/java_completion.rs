//! Member completion after `receiver.` in Java, ranked as jdtls ranks it.
//!
//! The receiver is typed from a speculative parse (a marker typed at the
//! cursor, see `indexer/infer/speculative.rs`), and its class is resolved
//! from the file that spelled the type: `assertThat(json)` returns Truth's
//! `StringSubject`, declared beside `Truth`, whatever else is named
//! `StringSubject` in the workspace or the JDK. The members are the class's
//! and its supertypes' (`Object`'s included), less what the caller can't
//! access: private ones outside their file, package-private ones outside
//! their package, protected ones outside both unless reached through `this`.
//! After a type name only static members and nested types are offered.
//!
//! Ranking follows jdtls's relevance: a member of the type the context
//! expects comes first (an argument's parameter type, a variable's declared
//! type, `boolean` for a condition), then instance members before static
//! ones; with a `(` already after the cursor, methods before fields, else
//! fields before methods; then by name.

use std::collections::{HashSet, VecDeque};

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Position, Range, SymbolKind, TextEdit,
    TypeHierarchyItem, Url,
};
use tree_sitter::Node;

use crate::features::implementation::{is_class_kind, range_contains};
use crate::features::parameter_hints::callee_declarations;
use crate::features::signature_help::{
    called_name, java_parameter_label, strip_declaration_prefix,
};
use crate::features::type_hierarchy::{declared_name, resolve_type, supertypes};
use crate::indexer::{
    closed_before, java_receiver_node_for_marker, speculative_doc, split_params_at_depth_zero,
    CstQuery, Indexer, LiveDoc, Resolution, COMPLETION_MARKER,
};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};
use crate::resolver::complete::completion_item_for_nested_symbol;
use crate::types::{CallerContext, CursorPos, SymbolEntry, Visibility};

/// How many supertypes deep members are collected.
const MAX_SUPERTYPES: usize = 32;

/// Relevance points, after jdtls's: an expected type outweighs the rest.
const EXPECTED_TYPE: u8 = 30;
const NON_STATIC: u8 = 11;
const PREFERRED_KIND: u8 = 5;
/// Declared by the receiver's own class rather than inherited.
const NON_INHERITED: u8 = 2;

/// Member completions for the Java `receiver.` at `position`, or `None` when
/// `uri` isn't Java, nothing before the cursor is a `receiver.`, or the
/// receiver's class can't be found — the general completion answers then.
/// `expected_arguments` are the parameter types the call around the cursor
/// may take there ([`expected_argument_types`]).
pub(crate) fn java_member_completions(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
    snippets: bool,
    expected_arguments: Vec<String>,
) -> Option<Vec<CompletionItem>> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let query = MemberQuery::at(indexer, uri, position, expected_arguments)?;
    let is_array = is_array_type(&query.receiver_type);
    let receiver_class = if is_array {
        "Object"
    } else {
        simple_type_name(&query.receiver_type)?
    };
    let receiver = resolve_type(indexer, receiver_class, &query.anchor);
    if is_array && receiver.is_none() {
        return Some(array_length_completion(&query.typed).into_iter().collect());
    }
    let receiver = receiver?;
    let caller_package = indexer.package_of(uri);
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    let mut items = Vec::new();
    for (owner, member) in members(indexer, &receiver) {
        let is_method = member.kind == SymbolKind::METHOD;
        let is_type = is_class_kind(member.kind);
        let is_static = is_type || is_static_member(&member);
        if query.static_access && !is_static {
            continue;
        }
        if !query.static_access && is_type {
            continue;
        }
        let member_package = indexer
            .file_data_for(owner.uri.as_str())
            .and_then(|data| data.package.clone());
        let access = Access {
            same_file: owner.uri == *uri,
            same_package: member_package == caller_package,
            through_this: query.through_this,
        };
        if !query.typed.admits(&member.name) {
            continue;
        }
        if !access.allows(member.visibility) || !seen.insert((member.name.clone(), is_method)) {
            continue;
        }
        let mut score = 0;
        if member_type(&member, &owner).is_some_and(|actual| {
            query
                .expected
                .iter()
                .any(|expected| same_type(expected, &actual))
        }) {
            score += EXPECTED_TYPE;
        }
        if !is_static {
            score += NON_STATIC;
        }
        if is_method == query.wants_call {
            score += PREFERRED_KIND;
        }
        let declared_by_receiver = owner.uri == receiver.uri && owner.range == receiver.range;
        if declared_by_receiver {
            score += NON_INHERITED;
        }
        let caller = CallerContext {
            uri: Some(uri.as_str()),
            cursor_line: Some(position.line),
        };
        let mut item =
            completion_item_for_nested_symbol(indexer, &member, owner.uri.as_str(), caller);
        item.sort_text = Some(format!("{:02}:{}", 99 - score, member.name));
        if !snippets {
            item.insert_text = None;
            item.insert_text_format = None;
        }
        items.push(item);
    }
    if is_array {
        items.extend(array_length_completion(&query.typed));
    }
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    Some(items)
}

/// Completions where Java names a package or a type by its qualified name:
/// the `package` declaration, an `import`, and `new a.b.Type`. `None`
/// anywhere else, or when nothing matches — the general completion answers
/// then.
pub(crate) fn java_qualified_name_completions(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<Vec<CompletionItem>> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let base = indexer.live_doc_or_parse(uri)?;
    let cursor = byte_of(&base.bytes, position)?;
    let line = line_before(&base.bytes, cursor)?;
    let site = QualifiedNameSite::of(line)?;
    let replaced = Range::new(
        Position::new(
            position.line,
            position.character.saturating_sub(site.typed_len_utf16()),
        ),
        position,
    );
    let mut items = Vec::new();
    if site.kind == QualifiedNameKind::PackageDeclaration {
        // The package the file's directory names, as jdtls offers it: the
        // one a new file is about to declare, before any file declares it.
        items.extend(
            package_of_directory(indexer, uri)
                .filter(|package| starts_with_ignoring_case(package, site.dotted))
                .map(|package| package_item(package, replaced)),
        );
    }
    if site.offers_packages() {
        items.extend(
            indexer
                .known_packages()
                .into_iter()
                .filter(|package| starts_with_ignoring_case(package, site.dotted))
                // The package being declared is in the index already, as
                // far as it is typed: offering it would complete nothing.
                .filter(|package| !package.eq_ignore_ascii_case(site.dotted))
                .map(|package| package_item(package, replaced)),
        );
    }
    if site.offers_types() {
        if let Some((package, simple)) = site.dotted.rsplit_once('.') {
            let typed = TypedPrefix(simple.to_owned());
            let mut types: Vec<(Url, SymbolEntry)> = indexer
                .top_level_types_in_package(package)
                .into_iter()
                .filter(|(_, symbol)| typed.admits(&symbol.name))
                .collect();
            types.sort_by(|(_, left), (_, right)| left.name.cmp(&right.name));
            types.dedup_by(|(_, left), (_, right)| left.name == right.name);
            items.extend(
                types
                    .into_iter()
                    .map(|(_, symbol)| type_item(&symbol, package)),
            );
        }
    }
    let mut labels = HashSet::new();
    items.retain(|item| labels.insert(item.label.clone()));
    (!items.is_empty()).then_some(items)
}

/// A line that names a package or type by its qualified name, up to the
/// cursor.
#[derive(Debug, PartialEq, Eq)]
struct QualifiedNameSite<'a> {
    kind: QualifiedNameKind,
    /// The qualified name typed so far: `com.demo.Ro` of `import com.demo.Ro`.
    dotted: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QualifiedNameKind {
    /// `package com.dem` — only packages.
    PackageDeclaration,
    /// `import com.demo.Ro` — packages, and the types of the package named
    /// before the last dot.
    Import,
    /// `new com.demo.Ro` — the types of the package named before the last
    /// dot.
    Instantiation,
}

impl<'a> QualifiedNameSite<'a> {
    fn of(line: &'a str) -> Option<Self> {
        let is_name_char = |char: char| char.is_alphanumeric() || matches!(char, '_' | '$' | '.');
        let dotted_start = line
            .char_indices()
            .rev()
            .take_while(|(_, char)| is_name_char(*char))
            .last()
            .map_or(line.len(), |(at, _)| at);
        let dotted = &line[dotted_start..];
        let keyword = line[..dotted_start].trim_end();
        let kind = if keyword.trim_start() == "package" {
            QualifiedNameKind::PackageDeclaration
        } else if keyword.trim_start() == "import" || keyword.trim_start() == "import static" {
            QualifiedNameKind::Import
        } else if ends_with_word(keyword, "new") && dotted.contains('.') {
            QualifiedNameKind::Instantiation
        } else {
            return None;
        };
        let separated_from_keyword = dotted_start > keyword.len();
        separated_from_keyword.then_some(Self { kind, dotted })
    }

    fn offers_packages(&self) -> bool {
        matches!(
            self.kind,
            QualifiedNameKind::PackageDeclaration | QualifiedNameKind::Import
        )
    }

    fn offers_types(&self) -> bool {
        matches!(
            self.kind,
            QualifiedNameKind::Import | QualifiedNameKind::Instantiation
        )
    }

    fn typed_len_utf16(&self) -> u32 {
        self.dotted.encode_utf16().count() as u32
    }
}

/// The package the directory of `uri` names: below its source set
/// (`src/main/java/`, …), or else below the workspace root.
fn package_of_directory(indexer: &Indexer, uri: &Url) -> Option<String> {
    if let Some(package) = crate::features::code_actions::resolve_package_from_path(uri.path()) {
        return Some(package);
    }
    let root = indexer.workspace_root.get()?;
    let file = uri.to_file_path().ok()?;
    let directory = file.strip_prefix(&root).ok()?.parent()?;
    let segments: Vec<&str> = directory
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let all_identifiers = segments.iter().all(|segment| is_java_identifier(segment));
    (!segments.is_empty() && all_identifiers).then(|| segments.join("."))
}

fn is_java_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let starts_well = chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$');
    starts_well && chars.all(|char| char.is_alphanumeric() || char == '_' || char == '$')
}

/// A package, labelled and inserted by its whole name over the qualified
/// name typed so far.
fn package_item(package: String, replaced: Range) -> CompletionItem {
    CompletionItem {
        label: package.clone(),
        kind: Some(CompletionItemKind::MODULE),
        filter_text: Some(package.clone()),
        sort_text: Some(format!("10:{package}")),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(replaced, package))),
        ..Default::default()
    }
}

/// A top-level type of `package`, named simply: what comes after the last
/// dot is the word being replaced.
fn type_item(symbol: &SymbolEntry, package: &str) -> CompletionItem {
    let kind = match symbol.kind {
        SymbolKind::INTERFACE => CompletionItemKind::INTERFACE,
        SymbolKind::ENUM => CompletionItemKind::ENUM,
        SymbolKind::STRUCT => CompletionItemKind::STRUCT,
        _ => CompletionItemKind::CLASS,
    };
    CompletionItem {
        label: symbol.name.clone(),
        kind: Some(kind),
        detail: Some(format!("{package}.{}", symbol.name)),
        sort_text: Some(format!("00:{}", symbol.name)),
        ..Default::default()
    }
}

/// Whether `line` ends in the whole word `word`.
fn ends_with_word(line: &str, word: &str) -> bool {
    line.strip_suffix(word).is_some_and(|before| {
        before
            .chars()
            .next_back()
            .is_none_or(|char| !(char.is_alphanumeric() || char == '_' || char == '$'))
    })
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.is_char_boundary(prefix.len())
        && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// The byte offset of the LSP `position` in `bytes`.
fn byte_of(bytes: &[u8], position: Position) -> Option<usize> {
    let starts = line_starts(bytes);
    let line_start = *starts.get(position.line as usize)?;
    let line_end = bytes[line_start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |at| line_start + at);
    let line = std::str::from_utf8(&bytes[line_start..line_end]).ok()?;
    let mut units = 0;
    for (offset, char) in line.char_indices() {
        if units >= position.character as usize {
            return Some(line_start + offset);
        }
        units += char.len_utf16();
    }
    Some(line_end)
}

/// The text of the line that holds `byte`, up to it.
fn line_before(bytes: &[u8], byte: usize) -> Option<&str> {
    let start = bytes[..byte]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    std::str::from_utf8(&bytes[start..byte]).ok()
}

/// The Java keywords that can start what is being typed at `position` and
/// match `typed`: declarations at the top of a file and in a class body,
/// statements in a block. None when nothing is typed yet, or when what
/// precedes the word on its line isn't the start of a declaration or a
/// statement (`x = fo|`, `new Bo|`).
pub(crate) fn java_keyword_completions(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Vec<CompletionItem> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return Vec::new();
    }
    let Some(base) = indexer.live_doc_or_parse(uri) else {
        return Vec::new();
    };
    let Some(cursor) = byte_of(&base.bytes, position) else {
        return Vec::new();
    };
    let typed = TypedPrefix::before(&base.bytes[..cursor]);
    if typed.0.is_empty() {
        return Vec::new();
    }
    let word_start = cursor - typed.0.len();
    let Some(line) = line_before(&base.bytes, word_start) else {
        return Vec::new();
    };
    if !starts_a_declaration_or_statement(line) {
        return Vec::new();
    }
    let scope = KeywordScope::at(&base, word_start);
    let nothing_before = base.bytes[..word_start].iter().all(u8::is_ascii_whitespace);
    scope
        .keywords()
        .into_iter()
        .filter(|keyword| *keyword != PACKAGE || nothing_before)
        .filter(|keyword| typed.admits(keyword))
        .map(|keyword| CompletionItem {
            label: (*keyword).to_owned(),
            kind: Some(CompletionItemKind::KEYWORD),
            sort_text: Some(format!("900:{keyword}")),
            ..Default::default()
        })
        .collect()
}

const PACKAGE: &str = "package";

const MODIFIERS: &[&str] = &[
    "public",
    "protected",
    "private",
    "static",
    "final",
    "abstract",
    "sealed",
    "synchronized",
    "native",
    "transient",
    "volatile",
    "default",
    "strictfp",
];

/// What a file may start a top-level type with.
const TOP_LEVEL_MODIFIERS: &[&str] = &["public", "abstract", "final", "sealed", "strictfp"];

const TYPE_DECLARATIONS: &[&str] = &["class", "interface", "enum", "record"];

const PRIMITIVE_TYPES: &[&str] = &[
    "void", "boolean", "byte", "char", "short", "int", "long", "float", "double",
];

const STATEMENT_KEYWORDS: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "return",
    "break",
    "continue",
    "try",
    "catch",
    "finally",
    "throw",
    "new",
    "final",
    "var",
    "this",
    "super",
    "true",
    "false",
    "null",
    "synchronized",
    "assert",
    "yield",
    "class",
    "record",
];

/// Where a keyword is being typed: what may be declared or done there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeywordScope {
    /// Outside every type: `package`, `import`, type declarations.
    File,
    /// Directly in a class, interface, enum or record body: members.
    TypeBody,
    /// In a method, constructor, initializer or lambda body: statements.
    Block,
}

impl KeywordScope {
    /// The innermost scope around `byte` in `doc`.
    fn at(doc: &LiveDoc, byte: usize) -> Self {
        let start = byte.saturating_sub(1);
        let Some(mut node) = doc.tree.root_node().descendant_for_byte_range(start, byte) else {
            return Self::File;
        };
        loop {
            match node.kind() {
                "block" | "constructor_body" | "switch_block" | "lambda_expression" => {
                    return Self::Block
                }
                "class_body"
                | "interface_body"
                | "enum_body"
                | "enum_body_declarations"
                | "annotation_type_body" => return Self::TypeBody,
                "program" => return Self::File,
                _ => {}
            }
            match node.parent() {
                Some(parent) => node = parent,
                None => return Self::File,
            }
        }
    }

    fn keywords(self) -> Vec<&'static str> {
        let groups: &[&[&'static str]] = match self {
            Self::File => &[&[PACKAGE, "import"], TOP_LEVEL_MODIFIERS, TYPE_DECLARATIONS],
            Self::TypeBody => &[MODIFIERS, TYPE_DECLARATIONS, PRIMITIVE_TYPES],
            Self::Block => &[STATEMENT_KEYWORDS, PRIMITIVE_TYPES],
        };
        groups.concat()
    }
}

/// Whether the text of a line before a word lets the word start a
/// declaration or a statement: nothing, the end of one (`;`, `{`, `}`), or
/// modifiers (`public sta|`).
fn starts_a_declaration_or_statement(line: &str) -> bool {
    let before = line.trim_end();
    if before.is_empty() || before.ends_with(['{', '}', ';']) {
        return true;
    }
    let last_word = before
        .rsplit(|char: char| !(char.is_alphanumeric() || char == '_' || char == '$'))
        .next()
        .unwrap_or_default();
    MODIFIERS.contains(&last_word)
}

/// Whether the word being typed follows `new`: only a type can be named
/// there, never a local or a member.
pub(crate) fn is_java_instantiation(uri: &Url, before_word: &str) -> bool {
    crate::Language::from_path(uri.path()) == crate::Language::Java
        && ends_with_word(before_word.trim_end(), "new")
}

/// Whether `type_text` is an array (`String[]`, `int[][]`, `T...`).
fn is_array_type(type_text: &str) -> bool {
    let unannotated = type_text.trim();
    unannotated.ends_with("[]") || unannotated.ends_with("...")
}

/// The `length` every array has (JLS §10.7). The rest of an array's
/// members are `Object`'s, read from wherever the JDK is.
fn array_length_completion(typed: &TypedPrefix) -> Option<CompletionItem> {
    const LENGTH: &str = "length";
    typed.admits(LENGTH).then(|| CompletionItem {
        label: LENGTH.to_owned(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some("int length".to_owned()),
        sort_text: Some(format!("{:02}:{LENGTH}", 99 - NON_STATIC)),
        ..Default::default()
    })
}

/// The constants of the enum a Java `switch` is on, when the cursor is a
/// `case` label's name (`case NU|`, `case A, B|`): Java names them there
/// unqualified, so they are the answer, not a ranking among the index's
/// matches. `None` when the cursor is not on a case label, the switch isn't
/// on an enum, or the subject's type doesn't resolve — the general
/// completion answers then.
pub(crate) fn java_case_label_completions(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
    snippets: bool,
) -> Option<Vec<CompletionItem>> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let (doc, marker_byte) = marked_doc(indexer, uri, position)?;
    let line_start = doc.bytes[..marker_byte]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    if !is_case_label(doc.bytes.get(line_start..marker_byte)?) {
        return None;
    }
    // Still being typed, the switch is often lost to an ERROR node: the
    // text up to the cursor, every bracket closed, parses it again.
    let closed;
    let doc = match switch_subject(&doc, marker_byte) {
        Some(_) => &doc,
        None => {
            let mut before = doc.bytes[..marker_byte + COMPLETION_MARKER.len()].to_vec();
            before.push(b':');
            closed = closed_before(&before)?;
            &closed
        }
    };
    let subject = switch_subject(doc, marker_byte)?;
    let resolved = match CstQuery::new(subject, doc, indexer, uri).expr_type() {
        Resolution::Resolved(resolved) => resolved,
        _ => return None,
    };
    let enum_type = resolve_type(
        indexer,
        simple_type_name(resolved.as_type_str())?,
        resolved.declaring_uri(),
    )?;
    let caller = CallerContext {
        uri: Some(uri.as_str()),
        cursor_line: Some(position.line),
    };
    let items: Vec<CompletionItem> = members(indexer, &enum_type)
        .into_iter()
        .filter(|(owner, member)| {
            member.kind == SymbolKind::ENUM_MEMBER
                && owner.uri == enum_type.uri
                && owner.range == enum_type.range
        })
        .map(|(owner, member)| {
            let mut item =
                completion_item_for_nested_symbol(indexer, &member, owner.uri.as_str(), caller);
            item.sort_text = Some(format!("00:{}", member.name));
            if !snippets {
                item.insert_text = None;
                item.insert_text_format = None;
            }
            item
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

/// Whether `line` (up to the cursor) ends in a `case` label's name being
/// typed: `case`, then names separated by commas, the last one partial.
fn is_case_label(line: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let text = text.trim_end_matches(is_word).trim_end();
    let mut rest = text;
    while let Some(before) = rest.strip_suffix(',') {
        // An earlier label of the same `case A, B`.
        rest = before.trim_end().trim_end_matches(is_word).trim_end();
    }
    rest.strip_suffix("case")
        .is_some_and(|before| before.is_empty() || before.ends_with(|c: char| !is_word(c)))
}

/// The expression a `switch` is on, from the switch around `marker_byte`.
fn switch_subject(doc: &LiveDoc, marker_byte: usize) -> Option<Node<'_>> {
    let mut node = doc
        .tree
        .root_node()
        .descendant_for_byte_range(marker_byte, marker_byte + COMPLETION_MARKER.len())?;
    loop {
        if matches!(node.kind(), "switch_expression" | "switch_statement") {
            let condition = node.child_by_field_name("condition")?;
            return if condition.kind() == "parenthesized_expression" {
                condition.named_child(0)
            } else {
                Some(condition)
            };
        }
        node = node.parent()?;
    }
}

/// The types of the parameter the call around `position` takes where the
/// cursor is — `String` for `separateCamelCase(f.|, ' ')` — one per overload
/// the call may be, as go-to-definition resolves it. Empty outside a Java
/// call's arguments.
pub(crate) async fn expected_argument_types(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Vec<String> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return Vec::new();
    }
    let Some((callee, argument)) = enclosing_call(indexer, uri, position) else {
        return Vec::new();
    };
    let mut types: Vec<String> = Vec::new();
    for declaration in callee_declarations(indexer, uri, callee).await {
        let Some(params) = declared_params(indexer, &declaration) else {
            continue;
        };
        if let Some(parameter) = parameter_type_at(&params, argument) {
            if !types.contains(&parameter) {
                types.push(parameter);
            }
        }
    }
    types
}

/// Where the call whose arguments hold the member access at `position` names
/// its callee, and the index of that argument.
fn enclosing_call(indexer: &Indexer, uri: &Url, position: Position) -> Option<(Position, usize)> {
    let (doc, marker_byte) = marked_doc(indexer, uri, position)?;
    let receiver = java_receiver_node_for_marker(&doc, marker_byte)?;
    let access = receiver.parent()?;
    let arguments = access
        .parent()
        .filter(|parent| parent.kind() == "argument_list")?;
    let callee = called_name(arguments.parent()?)?;
    let mut cursor = arguments.walk();
    let argument = arguments
        .children(&mut cursor)
        .filter(|child| child.kind() == "," && child.end_byte() <= access.start_byte())
        .count();
    let starts = line_starts(&doc.bytes);
    // The callee comes before the marker, where both documents agree.
    Some((
        ts_pos_to_lsp(callee.start_position(), &starts, &doc.bytes),
        argument,
    ))
}

/// The document with the completion marker typed at `position`, and where.
fn marked_doc(indexer: &Indexer, uri: &Url, position: Position) -> Option<(LiveDoc, usize)> {
    let base = indexer.live_doc_or_parse(uri)?;
    let language = crate::indexer::live_tree::lang_for_path(uri.path())?;
    speculative_doc(
        &base,
        language,
        CursorPos {
            line: position.line as usize,
            utf16_col: position.character as usize,
        },
    )
}

/// The member access being completed, as read from the marked document.
struct MemberQuery {
    /// The receiver's type, as written where it was declared.
    receiver_type: String,
    /// The file that spelled `receiver_type`: its name resolves there.
    anchor: Url,
    /// `Type.member`: only static members (and nested types) apply.
    static_access: bool,
    /// `this.member`: protected members are reachable.
    through_this: bool,
    /// The types the context expects the member's to be (one per overload
    /// of the call it is an argument of).
    expected: Vec<String>,
    /// A `(` already follows the cursor: a method is being named.
    wants_call: bool,
    /// The part of the member's name already typed before the cursor.
    typed: TypedPrefix,
}

impl MemberQuery {
    fn at(
        indexer: &Indexer,
        uri: &Url,
        position: Position,
        expected_arguments: Vec<String>,
    ) -> Option<Self> {
        let (doc, marker_byte) = marked_doc(indexer, uri, position)?;
        let receiver = java_receiver_node_for_marker(&doc, marker_byte)?;
        if receiver.kind() == "super" {
            return None;
        }
        let resolved = match CstQuery::new(receiver, &doc, indexer, uri).expr_type() {
            Resolution::Resolved(resolved) => resolved,
            _ => return None,
        };
        let receiver_type = resolved.as_type_str().to_owned();
        let receiver_text = receiver.utf8_text(&doc.bytes).ok()?;
        let static_access = names_type(receiver, receiver_text, &receiver_type);
        let access = receiver.parent()?;
        let expected = if expected_arguments.is_empty() {
            expected_type(access, &doc, indexer, uri)
                .into_iter()
                .collect()
        } else {
            expected_arguments
        };
        let after_marker = doc.bytes.get(marker_byte..)?;
        let wants_call = after_marker
            .iter()
            .skip(COMPLETION_MARKER.len())
            .skip_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(&b'(');
        let typed = TypedPrefix::before(doc.bytes.get(..marker_byte)?);
        Some(MemberQuery {
            receiver_type,
            anchor: resolved.declaring_uri().clone(),
            static_access,
            through_this: receiver.kind() == "this",
            expected,
            wants_call,
            typed,
        })
    }
}

/// The identifier being typed at the cursor, which a name has to match to
/// be offered — as jdtls filters, by prefix (any case) or by the initials
/// of its camel-case words (`mSF` for `myStateFlow`), not by a substring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedPrefix(String);

/// How well a name matches what was typed, from `match_score`.
const PREFIX_MATCH: u8 = 0;
const CAMEL_CASE_MATCH: u8 = 1;

impl TypedPrefix {
    /// The identifier characters at the end of `before`.
    pub(crate) fn before(before: &[u8]) -> Self {
        let start = before
            .iter()
            .rposition(|byte| !is_identifier_byte(*byte))
            .map_or(0, |at| at + 1);
        Self(String::from_utf8_lossy(&before[start..]).into_owned())
    }

    pub(crate) fn admits(&self, name: &str) -> bool {
        matches!(
            crate::resolver::complete::match_score(name, &self.0),
            Some(PREFIX_MATCH | CAMEL_CASE_MATCH)
        )
    }
}

/// Whether `receiver` is a type name (`Pattern` of `Pattern.compile`,
/// `Outer.Inner`) rather than a value of that type.
fn names_type(receiver: Node<'_>, text: &str, receiver_type: &str) -> bool {
    let named = text.rsplit('.').next().unwrap_or(text).trim();
    matches!(
        receiver.kind(),
        "identifier"
            | "type_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "field_access"
    ) && named.starts_with(char::is_uppercase)
        && simple_type_name(receiver_type) == Some(named)
}

/// The type the context of the member access `access` expects of it: the
/// declared type of the variable it initializes or is assigned to, the
/// enclosing method's return type, `boolean` in a condition.
fn expected_type(access: Node<'_>, doc: &LiveDoc, indexer: &Indexer, uri: &Url) -> Option<String> {
    const BOOLEAN: &str = "boolean";
    let parent = access.parent()?;
    let is_field = |name: &str| {
        parent
            .child_by_field_name(name)
            .is_some_and(|child| child.id() == access.id())
    };
    match parent.kind() {
        "variable_declarator" if is_field("value") => {
            let declaration = parent.parent()?;
            let declared = declaration.child_by_field_name("type")?;
            Some(declared.utf8_text(&doc.bytes).ok()?.to_owned())
                .filter(|declared| declared != "var")
        }
        "assignment_expression" if is_field("right") => {
            let left = parent.child_by_field_name("left")?;
            match CstQuery::new(left, doc, indexer, uri).expr_type() {
                Resolution::Resolved(declared) => Some(declared.as_type_str().to_owned()),
                _ => None,
            }
        }
        "return_statement" => {
            let mut ancestor = parent.parent();
            while let Some(node) = ancestor {
                match node.kind() {
                    "method_declaration" => {
                        let returned = node.child_by_field_name("type")?;
                        return Some(returned.utf8_text(&doc.bytes).ok()?.to_owned());
                    }
                    "lambda_expression" | "class_body" => return None,
                    _ => ancestor = node.parent(),
                }
            }
            None
        }
        "unary_expression" => {
            let operator = parent.child_by_field_name("operator")?;
            (operator.kind() == "!").then(|| BOOLEAN.to_owned())
        }
        "binary_expression" => {
            let operator = parent.child_by_field_name("operator")?;
            matches!(operator.kind(), "&&" | "||").then(|| BOOLEAN.to_owned())
        }
        "ternary_expression" if is_field("condition") => Some(BOOLEAN.to_owned()),
        "parenthesized_expression" => {
            let statement = parent.parent()?;
            matches!(
                statement.kind(),
                "if_statement" | "while_statement" | "do_statement"
            )
            .then(|| BOOLEAN.to_owned())
        }
        _ => None,
    }
}

/// Who may see a member, from where the completion is asked.
struct Access {
    same_file: bool,
    same_package: bool,
    through_this: bool,
}

impl Access {
    fn allows(&self, visibility: Visibility) -> bool {
        match visibility {
            Visibility::Public | Visibility::Internal => true,
            Visibility::Private => self.same_file,
            Visibility::Package => self.same_file || self.same_package,
            Visibility::Protected => self.same_file || self.same_package || self.through_this,
        }
    }
}

/// The members of `receiver` and of its supertypes, nearest first, each with
/// the type that declares it. Interfaces reach `Object` too, as every value
/// is one.
fn members(
    indexer: &Indexer,
    receiver: &TypeHierarchyItem,
) -> Vec<(TypeHierarchyItem, SymbolEntry)> {
    let mut found = Vec::new();
    let mut visited: HashSet<(Url, u32, u32)> = HashSet::new();
    let mut queue = VecDeque::from([receiver.clone()]);
    let mut reached_object = false;
    while let Some(owner) = queue.pop_front() {
        let key = (
            owner.uri.clone(),
            owner.selection_range.start.line,
            owner.selection_range.start.character,
        );
        if visited.len() >= MAX_SUPERTYPES || !visited.insert(key) {
            continue;
        }
        let Some(name) = declared_name(&owner) else {
            continue;
        };
        reached_object |= name == "Object";
        if let Some(data) = indexer.file_data_for(owner.uri.as_str()) {
            found.extend(
                data.symbols
                    .iter()
                    .filter(|symbol| {
                        symbol.container.as_deref() == Some(name.as_str())
                            && symbol.kind != SymbolKind::CONSTRUCTOR
                            && symbol.range != owner.range
                            && range_contains(owner.range, symbol.range)
                    })
                    .map(|symbol| (owner.clone(), symbol.clone())),
            );
        }
        queue.extend(supertypes(indexer, &owner));
        if queue.is_empty() && !reached_object {
            reached_object = true;
            queue.extend(resolve_type(indexer, "Object", &owner.uri));
        }
    }
    found
}

/// Whether a field or method is `static` (an enum constant always is).
fn is_static_member(member: &SymbolEntry) -> bool {
    match member.kind {
        SymbolKind::ENUM_MEMBER => true,
        _ => modifiers_of(member)
            .split_whitespace()
            .any(|word| word == "static"),
    }
}

/// The part of a member's detail before its name: its annotations,
/// modifiers, type parameters and type.
fn modifiers_of(member: &SymbolEntry) -> &str {
    let detail = member.detail.split('=').next().unwrap_or_default();
    match name_offset(detail, &member.name) {
        Some(offset) => &detail[..offset],
        None => "",
    }
}

/// Where `name` starts in `detail` as a whole word followed by `(` (a
/// method) or by nothing but whitespace, `,` or `;` (a field).
fn name_offset(detail: &str, name: &str) -> Option<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    detail
        .match_indices(name)
        .map(|(offset, _)| offset)
        .find(|&offset| {
            let before = detail[..offset].chars().next_back();
            let after = detail[offset + name.len()..].trim_start().chars().next();
            !before.is_some_and(is_word) && after.is_none_or(|c| matches!(c, '(' | ',' | ';'))
        })
}

/// A member's type: a method's return type, a field's declared type, an
/// enum constant's enum.
fn member_type(member: &SymbolEntry, owner: &TypeHierarchyItem) -> Option<String> {
    if member.kind == SymbolKind::ENUM_MEMBER {
        return declared_name(owner);
    }
    let declared = strip_declaration_prefix(modifiers_of(member)).trim();
    (!declared.is_empty()).then(|| declared.to_owned())
}

/// Whether the types `expected` and `actual` are the same class, compared
/// by simple name without type arguments, a primitive the same as its box.
fn same_type(expected: &str, actual: &str) -> bool {
    let key = |text: &str| -> Option<String> {
        let name = simple_type_name(text)?;
        let arrays = text.matches("[]").count();
        let boxed = match name {
            "int" => "Integer",
            "char" => "Character",
            "boolean" => "Boolean",
            "byte" => "Byte",
            "short" => "Short",
            "long" => "Long",
            "float" => "Float",
            "double" => "Double",
            other => other,
        };
        // A type variable (`T`) stands for any type: no match by name.
        let is_type_variable = boxed.len() == 1 && boxed.starts_with(char::is_uppercase);
        (!is_type_variable).then(|| format!("{boxed}{}", "[]".repeat(arrays)))
    };
    key(expected).is_some_and(|expected| key(actual) == Some(expected))
}

/// `Entry` of `java.util.Map.Entry<K, V>[]`.
fn simple_type_name(text: &str) -> Option<&str> {
    let raw = text.split('<').next()?.trim();
    let raw = raw.trim_end_matches("[]").trim_end_matches("...").trim();
    let name = raw.rsplit('.').next()?.trim();
    (!name.is_empty()).then_some(name)
}

/// A byte of a Java identifier; any byte of a non-ASCII character counts.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || !byte.is_ascii()
}

/// The parameter text of the method or constructor declared at `location`.
fn declared_params(indexer: &Indexer, location: &tower_lsp::lsp_types::Location) -> Option<String> {
    let data = indexer.file_data_for(location.uri.as_str())?;
    data.symbols
        .iter()
        .find(|symbol| symbol.selection_range == location.range)
        .map(|symbol| symbol.params.clone())
}

/// The type of the parameter that takes argument `index` of `params`: a
/// trailing varargs parameter's element type for any index past it.
fn parameter_type_at(params: &str, index: usize) -> Option<String> {
    let params = params.trim();
    let params = params.strip_prefix('(').unwrap_or(params);
    let params = params.strip_suffix(')').unwrap_or(params);
    let parameters: Vec<String> = split_params_at_depth_zero(params)
        .into_iter()
        .map(java_parameter_label)
        .filter(|label| !label.is_empty())
        .collect();
    let last = parameters.last()?;
    let varargs = last.contains("...");
    let parameter = match parameters.get(index) {
        Some(parameter) if !(varargs && index + 1 == parameters.len()) => parameter,
        _ if varargs && index + 1 >= parameters.len() => last,
        _ => return None,
    };
    // `String name` → `String`; `T... items` → `T`.
    let (declared, _name) = parameter.rsplit_once(' ')?;
    Some(declared.replace("...", "").trim().to_owned())
}

#[cfg(test)]
#[path = "java_completion_tests.rs"]
mod tests;

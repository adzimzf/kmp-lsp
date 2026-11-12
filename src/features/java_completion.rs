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
    CompletionItem, CompletionItemKind, Position, SymbolKind, TypeHierarchyItem, Url,
};
use tree_sitter::Node;

use crate::features::implementation::{is_class_kind, range_contains};
use crate::features::parameter_hints::callee_declarations;
use crate::features::signature_help::{
    called_name, java_parameter_label, strip_declaration_prefix,
};
use crate::features::type_hierarchy::{declared_name, resolve_type, supertypes};
use crate::indexer::{
    enclosing_java_class_names, java_locals_in_scope, java_receiver_node_for_marker,
    speculative_doc, split_params_at_depth_zero, CstQuery, Indexer, JavaLocal, JavaLocalBinding,
    LiveDoc, Resolution, COMPLETION_MARKER,
};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};
use crate::resolver::complete::{completion_item_for_nested_symbol, match_score};
use crate::types::{CallerContext, CursorPos, SymbolEntry, Visibility};

/// How many supertypes deep members are collected.
const MAX_SUPERTYPES: usize = 32;

/// Relevance points, after jdtls's: an expected type outweighs the rest.
const EXPECTED_TYPE: u8 = 30;
const NON_STATIC: u8 = 11;
const PREFERRED_KIND: u8 = 5;

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
    let receiver = resolve_type(
        indexer,
        simple_type_name(&query.receiver_type)?,
        &query.anchor,
    )?;
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
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    Some(items)
}

/// Bare-name completions in Java at `position` — what a name typed without
/// a receiver can mean there, ranked as jdtls ranks them: the locals and
/// parameters in scope, then the members of the enclosing class and its
/// supertypes (only static ones in a static method; private ones only from
/// this file), then the Java keywords the place allows. Types and
/// importable names come from the general bare-word walk, ranked after
/// these ([`BareRank::Other`]). `None` when `uri` isn't Java.
pub(crate) fn java_scope_completions(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
    prefix: &str,
    snippets: bool,
) -> Option<Vec<CompletionItem>> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let (doc, marker_byte) = marked_doc(indexer, uri, position)?;
    let cursor = doc
        .tree
        .root_node()
        .descendant_for_byte_range(marker_byte, marker_byte)?;
    let place = CodePlace::of(cursor);
    let mut completions = ScopeCompletions::new(prefix);
    for local in java_locals_in_scope(cursor, &doc.bytes) {
        completions.add_local(&local, &doc.bytes);
    }
    if let Some(class) = enclosing_java_class_names(cursor, &doc.bytes).first() {
        completions.add_members(indexer, uri, position, class, place, snippets);
    }
    completions.add_keywords(place);
    Some(completions.items)
}

/// `scope` (from [`java_scope_completions`]) followed by what the general
/// bare-word walk found that isn't already in it, in rank order.
pub(crate) fn rank_java_bare_completions(
    scope: Vec<CompletionItem>,
    walked: Vec<CompletionItem>,
) -> Vec<CompletionItem> {
    let offered: HashSet<String> = scope.iter().map(|item| item.label.clone()).collect();
    let mut items = scope;
    items.extend(
        walked
            .into_iter()
            .filter(|item| !offered.contains(&item.label))
            .map(|mut item| {
                let within = item.sort_text.take().unwrap_or_else(|| item.label.clone());
                item.sort_text = Some(BareRank::Other.sort_text(&within));
                item
            }),
    );
    items.sort_by(|left, right| left.sort_text.cmp(&right.sort_text));
    items
}

/// Where in a Java file a bare name is being typed: which keywords fit,
/// and which members of the enclosing class are reachable without `this`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CodePlace {
    /// Outside any class: `package`, `import`, a type declaration.
    TopLevel,
    /// In a class body, between members: modifiers and types.
    ClassBody,
    /// In a method, constructor, initializer or field initializer.
    Code { is_static: bool },
}

impl CodePlace {
    fn of(cursor: Node<'_>) -> Self {
        let mut in_code = false;
        let mut inner = cursor;
        while let Some(node) = inner.parent() {
            match node.kind() {
                "block" | "constructor_body" | "lambda_expression" => in_code = true,
                "variable_declarator" => {
                    in_code |= node
                        .child_by_field_name("value")
                        .is_some_and(|value| value.id() == inner.id());
                }
                "method_declaration" | "field_declaration" | "static_initializer" if in_code => {
                    let is_static =
                        node.kind() == "static_initializer" || has_static_modifier(node);
                    return CodePlace::Code { is_static };
                }
                "class_body"
                | "interface_body"
                | "enum_body"
                | "enum_body_declarations"
                | "annotation_type_body" => {
                    return if in_code {
                        CodePlace::Code { is_static: false }
                    } else {
                        CodePlace::ClassBody
                    };
                }
                _ => {}
            }
            inner = node;
        }
        CodePlace::TopLevel
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            CodePlace::TopLevel => TOP_LEVEL_KEYWORDS,
            CodePlace::ClassBody => CLASS_BODY_KEYWORDS,
            CodePlace::Code { .. } => CODE_KEYWORDS,
        }
    }
}

fn has_static_modifier(declaration: Node<'_>) -> bool {
    let mut cursor = declaration.walk();
    let found = declaration
        .children(&mut cursor)
        .filter(|child| child.kind() == "modifiers")
        .any(|modifiers| {
            let mut inner = modifiers.walk();
            let is_static = modifiers
                .children(&mut inner)
                .any(|modifier| modifier.kind() == "static");
            is_static
        });
    found
}

const TOP_LEVEL_KEYWORDS: &[&str] = &[
    "package",
    "import",
    "public",
    "abstract",
    "final",
    "sealed",
    "non-sealed",
    "class",
    "interface",
    "enum",
    "record",
];

const CLASS_BODY_KEYWORDS: &[&str] = &[
    "public",
    "protected",
    "private",
    "static",
    "final",
    "abstract",
    "synchronized",
    "native",
    "transient",
    "volatile",
    "default",
    "class",
    "interface",
    "enum",
    "record",
    "void",
    "boolean",
    "byte",
    "char",
    "short",
    "int",
    "long",
    "float",
    "double",
];

const CODE_KEYWORDS: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "return",
    "break",
    "continue",
    "try",
    "catch",
    "finally",
    "throw",
    "new",
    "this",
    "super",
    "null",
    "true",
    "false",
    "instanceof",
    "final",
    "var",
    "assert",
    "synchronized",
    "yield",
    "boolean",
    "byte",
    "char",
    "short",
    "int",
    "long",
    "float",
    "double",
];

/// The rank group of a bare-name completion in Java, best first.
#[derive(Clone, Copy)]
pub(crate) enum BareRank {
    Local,
    Member,
    /// What the general bare-word walk found: types, imports, other files.
    Other,
    Keyword,
}

impl BareRank {
    /// The `sort_text` of an item of this rank whose own order is `within`.
    pub(crate) fn sort_text(self, within: &str) -> String {
        let group = match self {
            BareRank::Local => 0,
            BareRank::Member => 1,
            BareRank::Other => 2,
            BareRank::Keyword => 3,
        };
        format!("{group}{within}")
    }
}

/// The bare-name completions collected so far, each name once.
struct ScopeCompletions<'prefix> {
    prefix: &'prefix str,
    seen: HashSet<String>,
    items: Vec<CompletionItem>,
}

impl<'prefix> ScopeCompletions<'prefix> {
    fn new(prefix: &'prefix str) -> Self {
        Self {
            prefix,
            seen: HashSet::new(),
            items: Vec::new(),
        }
    }

    /// The `sort_text` for `name` in `rank`, or `None` when it doesn't
    /// match the prefix or is already offered.
    fn admit(&mut self, name: &str, rank: BareRank) -> Option<String> {
        let score = match_score(name, self.prefix)?;
        if !self.seen.insert(name.to_owned()) {
            return None;
        }
        Some(rank.sort_text(&format!("{score}{}", name.to_lowercase())))
    }

    fn add_local(&mut self, local: &JavaLocal<'_>, bytes: &[u8]) {
        let Ok(name) = local.declared_name.utf8_text(bytes) else {
            return;
        };
        let Some(sort_text) = self.admit(name, BareRank::Local) else {
            return;
        };
        let detail = match &local.binding {
            JavaLocalBinding::Typed(declared) => Some(declared.clone()),
            JavaLocalBinding::Inferred(_) => Some("var".to_owned()),
            JavaLocalBinding::Untyped | JavaLocalBinding::NotLocal => None,
        };
        self.items.push(CompletionItem {
            label: name.to_owned(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail,
            sort_text: Some(sort_text),
            filter_text: Some(name.to_owned()),
            ..Default::default()
        });
    }

    fn add_members(
        &mut self,
        indexer: &Indexer,
        uri: &Url,
        position: Position,
        class: &str,
        place: CodePlace,
        snippets: bool,
    ) {
        let Some(receiver) = resolve_type(indexer, class, uri) else {
            return;
        };
        let only_static = matches!(place, CodePlace::Code { is_static: true });
        let in_code = matches!(place, CodePlace::Code { .. });
        let caller_package = indexer.package_of(uri);
        for (owner, member) in members(indexer, &receiver) {
            // Nested types come from the general walk; in a class body only
            // a member's name is never what is typed.
            if is_class_kind(member.kind) || !in_code {
                continue;
            }
            if only_static && !is_static_member(&member) {
                continue;
            }
            let member_package = indexer
                .file_data_for(owner.uri.as_str())
                .and_then(|data| data.package.clone());
            let access = Access {
                same_file: owner.uri == *uri,
                same_package: member_package == caller_package,
                through_this: true,
            };
            if !access.allows(member.visibility) {
                continue;
            }
            let Some(sort_text) = self.admit(&member.name, BareRank::Member) else {
                continue;
            };
            let caller = CallerContext {
                uri: Some(uri.as_str()),
                cursor_line: Some(position.line),
            };
            let mut item =
                completion_item_for_nested_symbol(indexer, &member, owner.uri.as_str(), caller);
            item.sort_text = Some(sort_text);
            item.filter_text = Some(member.name.clone());
            if !snippets {
                item.insert_text = None;
                item.insert_text_format = None;
            }
            self.items.push(item);
        }
    }

    fn add_keywords(&mut self, place: CodePlace) {
        for keyword in place.keywords() {
            let Some(sort_text) = self.admit(keyword, BareRank::Keyword) else {
                continue;
            };
            self.items.push(CompletionItem {
                label: (*keyword).to_owned(),
                kind: Some(CompletionItemKind::KEYWORD),
                sort_text: Some(sort_text),
                filter_text: Some((*keyword).to_owned()),
                ..Default::default()
            });
        }
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
        Some(MemberQuery {
            receiver_type,
            anchor: resolved.declaring_uri().clone(),
            static_access,
            through_this: receiver.kind() == "this",
            expected,
            wants_call,
        })
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

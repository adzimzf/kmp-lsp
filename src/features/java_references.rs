//! Find-references for a declaration in a Java file, answered the way the
//! Java compiler binds names (and so the way jdtls answers):
//!
//! - a usage in code counts when it *resolves* to the declaration — for a
//!   method, also when it resolves to a method the declaration overrides
//!   (`reader.endObject()` on a `JsonReader` may run `JsonTreeReader`'s
//!   override; `writer.endObject()` on a `JsonWriter` never does);
//! - for a type, a constructor call (`new Foo(..)`) is a usage, a
//!   constructor's own declaration (`Foo() {}`) is not;
//! - an `import` of the declaration counts;
//! - a javadoc reference (`{@link Foo#bar(int)}`, `@see Foo`) counts when it
//!   names the declaration; any other comment or string text does not.
//!
//! Occurrences are found by a workspace-wide word search and each one is
//! resolved with go-to-definition, so the answer is exactly as good as
//! go-to-definition is. Declarations this does not model (fields, locals,
//! anything outside a workspace Java file) return `None` and keep using
//! [`super::references`].

use std::collections::HashMap;

use tower_lsp::lsp_types::{
    DocumentHighlight, DocumentHighlightKind, GotoDefinitionResponse, Location, Position, Range,
    SymbolKind, Url,
};

use super::text_utils::utf16_column;
use crate::backend::cursor::CursorContext;
use crate::indexer::{
    Indexer, NotCodeKind, OccurrenceDetail, OccurrenceSite, SymbolAtCursor, SymbolRole,
};
use crate::resolver::{
    ReceiverTypeAgreement, Resolver, MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
};
use crate::types::SymbolEntry;

/// References to the Java declaration at (or used at) `position`, or `None`
/// when the declaration is not one this module answers for.
pub(crate) async fn find_java_references(
    index: &Indexer,
    uri: &Url,
    position: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    if !is_java_file(uri) || index.is_library_uri(uri) {
        return None;
    }
    let target = JavaTarget::at(index, uri, position).await?;
    let file_path = uri.to_file_path().ok();
    let (workspace_root, source_roots, matcher) = index.rg_scope_for_path(file_path.as_deref());
    let workspace_root = workspace_root?;
    let name = target.symbol.name.clone();
    let occurrences = tokio::task::spawn_blocking(move || {
        crate::rg::rg_word_occurrences(&name, &workspace_root, &source_roots)
    })
    .await
    .ok()?;
    let occurrences = match matcher.as_deref() {
        Some(matcher) => {
            let kept = matcher.filter_locs(
                occurrences
                    .iter()
                    .map(|(location, _)| location.clone())
                    .collect(),
            );
            occurrences
                .into_iter()
                .filter(|(location, _)| kept.contains(location))
                .collect()
        }
        None => occurrences,
    };
    let mut found =
        resolved_occurrences(index, &target, occurrences, include_declaration, false).await;
    // Usages spelled another way (Kotlin's `account.balance` for
    // `getBalance()`) and those inside library sources.
    for location in super::references::beyond_rg_reference_locations(index, uri, position).await {
        if !found.contains(&location) {
            found.push(location);
        }
    }
    Some(found)
}

/// `textDocument/documentHighlight` for a Java declaration: its occurrences
/// in `uri` itself, found as its references are — its own name (a write)
/// included when it is declared there. `None` when the declaration is not one
/// this module answers for.
pub(crate) async fn java_document_highlights(
    index: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<Vec<DocumentHighlight>> {
    if !is_java_file(uri) || index.is_library_uri(uri) {
        return None;
    }
    let target = JavaTarget::at(index, uri, position).await?;
    let lines = index.lines_for(uri)?;
    let occurrences = word_occurrences(uri, &lines, &target.symbol.name);
    let declared_here = target.location.uri == *uri;
    // A highlight marks what binds to this very method, as jdtls does.
    let found = resolved_occurrences(index, &target, occurrences, declared_here, true).await;
    Some(
        found
            .into_iter()
            .map(|location| DocumentHighlight {
                kind: Some(if location == target.location {
                    DocumentHighlightKind::WRITE
                } else {
                    DocumentHighlightKind::READ
                }),
                range: location.range,
            })
            .collect(),
    )
}

/// What renaming a Java declaration has to change.
pub(crate) enum JavaRename {
    /// Every name to change: the declaration's own, its references', the
    /// rest of a method's override family, and a type's constructors.
    Names(Vec<Location>),
    /// Why it can't be renamed without breaking code: a method in an override
    /// family this rename can't change whole.
    Refused(&'static str),
}

/// The most methods an override family may have for a rename to change them
/// all: each one is a reference search of its own.
const MAX_RENAMED_OVERRIDE_FAMILY: usize = 16;

/// What renaming the Java type or method at (or used at) `position` has to
/// change. `None` when the declaration is not one this module answers for.
///
/// A method is renamed with its whole override family — the workspace
/// methods it overrides and every method overriding those — and the
/// references of each, so every `@Override` keeps overriding. A family this
/// can't rename whole is refused: one reaching a library method, an
/// anonymous class's method (its family is its supertype's, any size), and
/// one too large to search.
pub(crate) async fn java_rename(
    index: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<JavaRename> {
    if !is_java_file(uri) || index.is_library_uri(uri) {
        return None;
    }
    let target = JavaTarget::at(index, uri, position).await?;
    let mut family = Vec::new();
    if target.kind == TargetKind::Method {
        family = match override_family(index, &target) {
            Ok(family) => family,
            Err(reason) => return Some(JavaRename::Refused(reason)),
        };
    }
    let mut locations = find_java_references(index, uri, position, true).await?;
    for member in family {
        let references = find_java_references(index, &member.uri, member.range.start, true).await;
        for location in references.unwrap_or_else(|| vec![member.clone()]) {
            if !locations.contains(&location) {
                locations.push(location);
            }
        }
    }
    if target.kind == TargetKind::Type {
        let constructors = index
            .file_symbols(&target.location.uri)
            .into_iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::CONSTRUCTOR
                    && symbol.name == target.symbol.name
                    && range_contains(target.symbol.range, symbol.selection_range.start)
            })
            .map(|symbol| Location::new(target.location.uri.clone(), symbol.selection_range));
        for constructor in constructors {
            if !locations.contains(&constructor) {
                locations.push(constructor);
            }
        }
    }
    Some(JavaRename::Names(locations))
}

/// The other members of the method `target`'s override family: the
/// workspace methods it overrides, and every method overriding the topmost
/// of them. Why not, when renaming them all is not possible.
fn override_family(index: &Indexer, target: &JavaTarget) -> Result<Vec<Location>, &'static str> {
    if index.is_anonymous_class_member(&target.location.uri, target.symbol.range) {
        return Err(
            "a method of an anonymous class overrides its supertype's — \
                    rename that method instead",
        );
    }
    let overridden = index.overridden_method_declarations(&target.symbol, &target.location);
    let is_workspace = |location: &Location| {
        location.uri.scheme() == "file" && !index.is_library_uri(&location.uri)
    };
    if !overridden.iter().all(is_workspace) {
        return Err("overrides a library method — cannot rename it");
    }
    let root_type = match overridden.last() {
        Some(root) => index
            .symbol_declared_at(&target.symbol.name, root)
            .and_then(|symbol| symbol.container),
        None => target.symbol.container.clone(),
    };
    let mut family = overridden;
    if let Some(root_type) = root_type {
        let overriding = index.overriding_method_declarations(
            &root_type,
            &target.symbol.name,
            target.symbol.param_counts.1,
            MAX_RENAMED_OVERRIDE_FAMILY + 1,
        );
        for declaration in overriding {
            if declaration != target.location && !family.contains(&declaration) {
                family.push(declaration);
            }
        }
    }
    if family.len() + 1 > MAX_RENAMED_OVERRIDE_FAMILY {
        return Err("too many methods override it to rename them all");
    }
    if !family.iter().all(is_workspace) {
        return Err("a library method overrides it — cannot rename it");
    }
    Ok(family)
}

/// Whole-word occurrences of `name` in `lines`, as `rg` reports them: the
/// location's column a byte offset, with the line's text.
fn word_occurrences(uri: &Url, lines: &[String], name: &str) -> Vec<(Location, String)> {
    let is_word =
        |character: char| character.is_alphanumeric() || character == '_' || character == '$';
    let mut found = Vec::new();
    for (number, line) in lines.iter().enumerate() {
        for (column, _) in line.match_indices(name) {
            let before = line[..column].chars().next_back();
            let after = line[column + name.len()..].chars().next();
            if before.is_some_and(is_word) || after.is_some_and(is_word) {
                continue;
            }
            let position = Position::new(number as u32, column as u32);
            found.push((
                Location::new(uri.clone(), Range::new(position, position)),
                line.clone(),
            ));
        }
    }
    found
}

/// The occurrences that name `target`, in the order found, its own
/// declaration first when `include_declaration`.
async fn resolved_occurrences(
    index: &Indexer,
    target: &JavaTarget,
    occurrences: Vec<(Location, String)>,
    include_declaration: bool,
    exact: bool,
) -> Vec<Location> {
    let occurrences: Vec<Occurrence> = occurrences
        .into_iter()
        .filter_map(|(rg_location, line)| Occurrence::new(rg_location, &line, &target.symbol.name))
        .filter(|occurrence| {
            let is_target = occurrence.location.uri == target.location.uri
                && occurrence.location.range.start == target.location.range.start;
            !is_target
        })
        .collect();
    let locations: Vec<&Location> = occurrences
        .iter()
        .map(|occurrence| &occurrence.location)
        .collect();
    let classified =
        crate::indexer::classify_occurrences(index, &locations, OccurrenceDetail::SiteOnly);

    let mut matcher = TargetMatcher {
        index,
        target,
        exact,
        resolved: HashMap::new(),
    };
    let mut found: Vec<Location> = Vec::new();
    if include_declaration {
        found.push(target.location.clone());
    }
    for (occurrence, classified) in occurrences.into_iter().zip(classified) {
        if matcher.accepts(&occurrence, &classified.site).await
            && !found.contains(&occurrence.location)
        {
            found.push(occurrence.location);
        }
    }
    found
}

fn is_java_file(uri: &Url) -> bool {
    uri.path().ends_with(".java")
}

// ─── The declaration ─────────────────────────────────────────────────────────

/// What a reference may resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    /// A class, interface, enum, record or annotation type.
    Type,
    /// A method (not a constructor).
    Method,
}

struct JavaTarget {
    kind: TargetKind,
    symbol: SymbolEntry,
    /// The declaration's name.
    location: Location,
    /// `com.example.Outer.Inner` for a type; the declaring type's for a method.
    type_fqn: String,
    /// The package of the declaring file.
    package: String,
}

impl JavaTarget {
    /// The declaration the cursor is on, or the one the name under it resolves to.
    async fn at(index: &Indexer, uri: &Url, position: Position) -> Option<Self> {
        let context = CursorContext::build(index, uri, position)?;
        let declared_here = index
            .symbol_declared_at(
                &context.word,
                &Location::new(uri.clone(), Range::new(position, position)),
            )
            .filter(|symbol| range_contains(symbol.selection_range, position));
        let location = match declared_here {
            Some(symbol) => Location::new(uri.clone(), symbol.selection_range),
            None => {
                let definitions =
                    crate::features::definition::find_definition(&context, index, uri, position)
                        .await?;
                definition_locations(definitions).into_iter().next()?
            }
        };
        if !is_java_file(&location.uri) || index.is_library_uri(&location.uri) {
            return None;
        }
        let symbol = index.symbol_declared_at(&context.word, &location)?;
        let kind = match symbol.kind {
            SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::ENUM
            | SymbolKind::STRUCT
            | SymbolKind::OBJECT => TargetKind::Type,
            SymbolKind::METHOD | SymbolKind::FUNCTION => TargetKind::Method,
            _ => return None,
        };
        if kind == TargetKind::Method && symbol.container.is_none() {
            return None;
        }
        let package = index.package_of(&location.uri).unwrap_or_default();
        let type_fqn = type_fqn(index, &location.uri, &package, &symbol, kind);
        Some(Self {
            kind,
            location: Location::new(location.uri.clone(), symbol.selection_range),
            symbol,
            type_fqn,
            package,
        })
    }

    /// The simple name of the type a reference has to name: the type itself,
    /// or a method's declaring type.
    fn type_name(&self) -> &str {
        self.type_fqn.rsplit('.').next().unwrap_or(&self.type_fqn)
    }

    /// The declaring type's own declaration, for a method target.
    fn declaring_type(&self) -> Option<&str> {
        self.symbol.container.as_deref()
    }
}

/// `package.Outer.Inner` for the type `symbol` is (a type target) or is
/// declared in (a method target), walking `container` links up the file.
fn type_fqn(
    index: &Indexer,
    uri: &Url,
    package: &str,
    symbol: &SymbolEntry,
    kind: TargetKind,
) -> String {
    let symbols = index.file_symbols(uri);
    let mut chain: Vec<String> = Vec::new();
    let mut current = match kind {
        TargetKind::Type => Some(symbol.name.clone()),
        TargetKind::Method => symbol.container.clone(),
    };
    while let Some(name) = current {
        current = symbols
            .iter()
            .find(|candidate| candidate.name == name && is_type_kind(candidate.kind))
            .and_then(|candidate| candidate.container.clone());
        chain.push(name);
        if chain.len() > 16 {
            break;
        }
    }
    chain.reverse();
    if package.is_empty() {
        chain.join(".")
    } else {
        format!("{package}.{}", chain.join("."))
    }
}

fn is_type_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::ENUM
            | SymbolKind::STRUCT
            | SymbolKind::OBJECT
    )
}

fn range_contains(range: Range, position: Position) -> bool {
    range.start <= position && position <= range.end
}

fn definition_locations(response: GotoDefinitionResponse) -> Vec<Location> {
    match response {
        GotoDefinitionResponse::Scalar(location) => vec![location],
        GotoDefinitionResponse::Array(locations) => locations,
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| Location::new(link.target_uri, link.target_selection_range))
            .collect(),
    }
}

// ─── One occurrence of the name ──────────────────────────────────────────────

/// One whole-word occurrence of the name, and what its line shows about it.
struct Occurrence {
    location: Location,
    /// The imported path, when the occurrence is in an `import` line.
    import_path: Option<String>,
    /// The javadoc reference covering the occurrence, if any.
    javadoc: Option<JavadocReference>,
    /// Written as a call (`name(`) or a method reference (`::name`).
    is_call_or_method_reference: bool,
}

impl Occurrence {
    /// `rg_location`'s column is a byte offset into `line`.
    fn new(rg_location: Location, line: &str, name: &str) -> Option<Self> {
        let byte_column = rg_location.range.start.character as usize;
        if line.get(byte_column..byte_column + name.len()) != Some(name) {
            return None;
        }
        let start = utf16_column(&line[..byte_column]);
        let line_number = rg_location.range.start.line;
        let location = Location::new(
            rg_location.uri,
            Range::new(
                Position::new(line_number, start),
                Position::new(line_number, start + utf16_column(name)),
            ),
        );
        let is_call = line[byte_column + name.len()..]
            .trim_start()
            .starts_with('(');
        let is_method_reference = line[..byte_column].trim_end().ends_with("::");
        Some(Self {
            location,
            import_path: import_path(line),
            javadoc: javadoc_reference_at(line, byte_column, name),
            is_call_or_method_reference: is_call || is_method_reference,
        })
    }
}

/// `a.b.C` for `import a.b.C;` and `import static a.b.C;`.
fn import_path(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("import ")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("static ").unwrap_or(rest);
    Some(rest.trim().trim_end_matches(';').trim().to_owned())
}

// ─── Javadoc references ──────────────────────────────────────────────────────

/// A javadoc reference (`{@link pkg.Type#member(int, String)}`, `@see Type`),
/// and which part of it the occurrence is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct JavadocReference {
    /// `pkg.Type`, `Type` or empty (`#member`).
    type_part: String,
    /// The name after `#`, if any.
    member: Option<String>,
    /// Parameter types written after the member, if any.
    parameters: Option<Vec<String>>,
    /// The occurrence is the last segment of `type_part`.
    names_type: bool,
    /// The occurrence is `member`.
    names_member: bool,
}

const JAVADOC_REFERENCE_TAGS: &[&str] = &[
    "{@link ",
    "{@linkplain ",
    "@see ",
    "@throws ",
    "@exception ",
];

/// The javadoc reference whose text covers `byte_column`, if any.
fn javadoc_reference_at(line: &str, byte_column: usize, name: &str) -> Option<JavadocReference> {
    let (tag_end, _) = JAVADOC_REFERENCE_TAGS
        .iter()
        .filter_map(|tag| {
            line[..byte_column]
                .rfind(tag)
                .map(|start| (start + tag.len(), *tag))
        })
        .max_by_key(|(end, _)| *end)?;
    let text = line[tag_end..].trim_start();
    let text_start = line.len() - text.len();
    let reference_end = reference_text_end(text);
    let reference = &text[..reference_end];
    if byte_column < text_start || byte_column + name.len() > text_start + reference_end {
        return None;
    }
    let offset = byte_column - text_start;
    let (type_part, member_part) = match reference.find('#') {
        Some(hash) => (&reference[..hash], Some(&reference[hash + 1..])),
        None => (reference, None),
    };
    let names_type = member_part.is_none_or(|_| offset < type_part.len())
        && offset + name.len() == type_part.len();
    let (member, parameters, names_member) = match member_part {
        Some(member_text) => {
            let member_start = type_part.len() + 1;
            let (member, parameters) = match member_text.find('(') {
                Some(open) => {
                    let close = member_text.rfind(')').unwrap_or(member_text.len());
                    let parameters = member_text[open + 1..close.max(open + 1)]
                        .split(',')
                        .map(|parameter| parameter.trim().to_owned())
                        .filter(|parameter| !parameter.is_empty())
                        .collect();
                    (&member_text[..open], Some(parameters))
                }
                None => (member_text, None),
            };
            let names_member = offset == member_start && member == name;
            (Some(member.to_owned()), parameters, names_member)
        }
        None => (None, None, false),
    };
    Some(JavadocReference {
        type_part: type_part.to_owned(),
        member,
        parameters,
        names_type,
        names_member,
    })
}

/// Where the reference text that starts `text` ends: at whitespace or `}`
/// outside a parameter list.
fn reference_text_end(text: &str) -> usize {
    let mut depth = 0usize;
    for (index, char) in text.char_indices() {
        match char {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '}' if depth == 0 => return index,
            char if char.is_whitespace() && depth == 0 => return index,
            _ => {}
        }
    }
    text.len()
}

/// `java.util.List<String>` → `List`, `int...` → `int[]`, `final Foo foo` → `Foo`.
fn simple_type_name(parameter: &str) -> String {
    let without_generics = strip_generics(parameter);
    let words: Vec<&str> = without_generics
        .split_whitespace()
        .filter(|word| *word != "final" && !word.starts_with('@'))
        .collect();
    let type_text = words.first().copied().unwrap_or("");
    let type_text = type_text.replace("...", "[]");
    type_text
        .rsplit('.')
        .next()
        .unwrap_or(&type_text)
        .to_owned()
}

fn strip_generics(text: &str) -> String {
    let mut depth = 0usize;
    let mut out = String::with_capacity(text.len());
    for char in text.chars() {
        match char {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            char if depth == 0 => out.push(char),
            _ => {}
        }
    }
    out
}

/// The simple type names of a declaration's parameters (`"int a, String b"`).
fn declared_parameter_types(params: &str) -> Vec<String> {
    split_top_level(params)
        .into_iter()
        .map(|parameter| {
            let parameter = strip_generics(parameter);
            let words: Vec<&str> = parameter
                .split_whitespace()
                .filter(|word| *word != "final" && !word.starts_with('@'))
                .collect();
            let type_words = &words[..words.len().saturating_sub(1)];
            simple_type_name(&type_words.join(" "))
        })
        .collect()
}

fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, char) in text.char_indices() {
        match char {
            '<' | '(' => depth += 1,
            '>' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(text[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

// ─── Matching an occurrence against the declaration ──────────────────────────

struct TargetMatcher<'a> {
    index: &'a Indexer,
    target: &'a JavaTarget,
    /// Only a use bound to the target itself counts — not one bound to a
    /// method it overrides, which may or may not run it.
    exact: bool,
    /// Whether a go-to-definition result is (or is overridden by) the target.
    resolved: HashMap<(String, u32), bool>,
}

impl TargetMatcher<'_> {
    async fn accepts(&mut self, occurrence: &Occurrence, site: &OccurrenceSite) -> bool {
        match site {
            OccurrenceSite::NotCode(NotCodeKind::Comment) => {
                occurrence.javadoc.as_ref().is_some_and(|reference| {
                    self.javadoc_names_target(reference, &occurrence.location.uri)
                })
            }
            OccurrenceSite::NotCode(_) => false,
            // Another declaration of the name — an overload, an override, a
            // constructor — declares something; it doesn't use the target.
            OccurrenceSite::Symbol(SymbolAtCursor {
                role: SymbolRole::Declaration { .. },
                ..
            }) => false,
            OccurrenceSite::Symbol(SymbolAtCursor {
                role: SymbolRole::ImportSegment,
                ..
            }) => occurrence
                .import_path
                .as_deref()
                .is_some_and(|path| self.import_names_target(path)),
            OccurrenceSite::Symbol(_) | OccurrenceSite::Unclassified => {
                if let Some(path) = occurrence.import_path.as_deref() {
                    return self.import_names_target(path);
                }
                // A method is only ever used by calling it or referring to it
                // with `::`; the bare name is a field or a local.
                if self.target.kind == TargetKind::Method && !occurrence.is_call_or_method_reference
                {
                    return false;
                }
                self.code_resolves_to_target(&occurrence.location).await
            }
        }
    }

    fn import_names_target(&self, path: &str) -> bool {
        let name = &self.target.symbol.name;
        match self.target.kind {
            TargetKind::Type => path == self.target.type_fqn,
            TargetKind::Method => path == format!("{}.{name}", self.target.type_fqn),
        }
    }

    fn javadoc_names_target(&self, reference: &JavadocReference, from: &Url) -> bool {
        match self.target.kind {
            TargetKind::Type => {
                reference.names_type && self.type_part_names_target(&reference.type_part, from)
            }
            TargetKind::Method => {
                if !reference.names_member {
                    return false;
                }
                let type_matches = if reference.type_part.is_empty() {
                    *from == self.target.location.uri
                } else {
                    self.type_part_names_target(&reference.type_part, from)
                        || self.type_part_is_supertype(&reference.type_part)
                };
                type_matches && self.parameters_match(reference.parameters.as_deref())
            }
        }
    }

    /// Whether the type written in a javadoc reference in `from` is the
    /// target's type: by its qualified name, or by a simple name visible there.
    fn type_part_names_target(&self, type_part: &str, from: &Url) -> bool {
        let fqn = &self.target.type_fqn;
        if type_part.contains('.') {
            return fqn == type_part || fqn.ends_with(&format!(".{type_part}"));
        }
        if type_part != self.target.type_name() {
            return false;
        }
        if *from == self.target.location.uri
            || self.index.package_of(from).as_deref() == Some(&self.target.package)
        {
            return true;
        }
        let Some(lines) = self.index.lines_for(from) else {
            return false;
        };
        let outer = fqn
            .rsplit_once('.')
            .map(|(outer, _)| outer)
            .unwrap_or_default();
        lines.iter().any(|line| {
            let line = line.trim();
            let Some(path) = line.strip_prefix("import ") else {
                return false;
            };
            let path = path.trim().trim_end_matches(';').trim();
            path == fqn || path == format!("{outer}.*")
        })
    }

    /// Whether a javadoc reference's type is a supertype of the method
    /// target's declaring type: `{@link TypeAdapter#read}` documents the
    /// method every adapter's `read` overrides.
    fn type_part_is_supertype(&self, type_part: &str) -> bool {
        let Some(declaring_type) = self.target.declaring_type() else {
            return false;
        };
        let supertype = type_part.rsplit('.').next().unwrap_or(type_part);
        matches!(
            self.index.receiver_type_agreement(
                declaring_type,
                self.target.location.uri.as_str(),
                supertype,
                MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
            ),
            ReceiverTypeAgreement::Inherited
        )
    }

    fn parameters_match(&self, written: Option<&[String]>) -> bool {
        let Some(written) = written else {
            return true;
        };
        let declared = declared_parameter_types(&self.target.symbol.params);
        written.len() == declared.len()
            && written
                .iter()
                .zip(&declared)
                .all(|(written, declared)| simple_type_name(written) == *declared)
    }

    async fn code_resolves_to_target(&mut self, location: &Location) -> bool {
        let Some(context) = CursorContext::build(self.index, &location.uri, location.range.start)
        else {
            return false;
        };
        let Some(response) = crate::features::definition::find_definition(
            &context,
            self.index,
            &location.uri,
            location.range.start,
        )
        .await
        else {
            return false;
        };
        definition_locations(response)
            .iter()
            .any(|definition| self.definition_is_target(definition))
    }

    fn definition_is_target(&mut self, definition: &Location) -> bool {
        let key = (definition.uri.to_string(), definition.range.start.line);
        if let Some(known) = self.resolved.get(&key) {
            return *known;
        }
        let answer = self.definition_is_target_uncached(definition);
        self.resolved.insert(key, answer);
        answer
    }

    fn definition_is_target_uncached(&self, definition: &Location) -> bool {
        let target = self.target;
        if definition.uri == target.location.uri
            && definition.range.start.line == target.location.range.start.line
        {
            return true;
        }
        let name = &target.symbol.name;
        let Some(symbol) = self.index.symbol_declared_at(name, definition) else {
            return false;
        };
        match target.kind {
            // `new Foo(..)` resolves to one of `Foo`'s constructors.
            TargetKind::Type => {
                definition.uri == target.location.uri
                    && symbol.container.as_deref() == Some(name.as_str())
                    && range_contains(target.symbol.range, definition.range.start)
            }
            TargetKind::Method => !self.exact && self.is_overridden_by_target(&symbol, definition),
        }
    }

    /// Whether `symbol` (a method named like the target, declared at
    /// `definition`) is one the target overrides: same parameter count, in a
    /// supertype of the target's declaring type.
    fn is_overridden_by_target(&self, symbol: &SymbolEntry, definition: &Location) -> bool {
        if symbol.param_counts.1 != self.target.symbol.param_counts.1 {
            return false;
        }
        let Some(supertype) = symbol.container.as_deref() else {
            return false;
        };
        // Every class extends `java.lang.Object`, which the hierarchy walk
        // never names: a call that binds to `Object`'s own method may run the
        // target's override.
        // The JDK's sources are indexed beside the workspace's, so being
        // indexed doesn't make `Object.java` a workspace file: a `jar:` entry
        // or a library file never is.
        let is_workspace_file = definition.uri.scheme() == "file"
            && !self.index.is_library_uri(&definition.uri)
            && !self.index.file_symbols(&definition.uri).is_empty();
        if supertype == "Object" && !is_workspace_file {
            return true;
        }
        let Some(declaring_type) = self.target.declaring_type() else {
            return false;
        };
        matches!(
            self.index.receiver_type_agreement(
                declaring_type,
                self.target.location.uri.as_str(),
                supertype,
                MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
            ),
            ReceiverTypeAgreement::Inherited
        )
    }
}

#[cfg(test)]
#[path = "java_references_tests.rs"]
mod tests;

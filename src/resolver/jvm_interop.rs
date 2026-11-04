//! Java ↔ Kotlin JVM interop names.
//!
//! The same declaration is spelled differently on each side of the language
//! boundary, because Java sees what the Kotlin compiler emits and Kotlin sees
//! Java's accessors as synthetic properties:
//!
//! | Kotlin declaration            | Java spelling                   |
//! |-------------------------------|---------------------------------|
//! | `val name`                    | `getName()`                     |
//! | `var email`                   | `getEmail()` / `setEmail(…)`    |
//! | `val isActive`                | `isActive()` / `setActive(…)`   |
//! | `@JvmName("removeAll") fun clear()` | `removeAll()`             |
//! | top-level fun in `Utils.kt`   | `UtilsKt.fn()`                  |
//! | `@file:JvmName("Text")`       | `Text.fn()`                     |
//! | `object Registry`             | `Registry.INSTANCE`             |
//!
//! | Java declaration      | Kotlin spelling                 |
//! |-----------------------|---------------------------------|
//! | `getBalance()`        | `.balance` (read)               |
//! | `setBalance(long)`    | `.balance = …` (write)          |
//! | `isLocked()`          | `.isLocked`                     |
//!
//! Everything here is a pure function of names, source lines and paths; the
//! resolver and find-references decide *when* a mapping applies.

use std::sync::Arc;

use tower_lsp::lsp_types::{Location, Position, SymbolKind, Url};

use crate::indexer::Indexer;
use crate::types::{FileData, Language, SymbolEntry, Visibility};

use super::container::range_encloses;
use super::resolve::ensure_file_data;

/// The Java name of the singleton field a Kotlin `object` compiles to.
pub(crate) const OBJECT_INSTANCE_FIELD: &str = "INSTANCE";

/// Whether a member access reads a value or assigns to it. Decides which of
/// a property's two accessors (`getX` / `setX`) a Kotlin `obj.x` stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyAccess {
    Read,
    Write,
}

/// A Java accessor method name decomposed into the property it accesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JavaAccessor {
    /// Kotlin property names this accessor may belong to, most likely first.
    /// `setActive` belongs to either `var active` or `var isActive`.
    pub(crate) property_names: Vec<String>,
    pub(crate) access: PropertyAccess,
}

/// The language a call site or declaration is written in, from its URI.
/// A JAR entry compiled from a `.class` counts as Java: Kotlin metadata isn't
/// visible to the interop rules there.
pub(crate) fn language_of(uri: &Url) -> Language {
    if uri.as_str().ends_with(".class") {
        return Language::Java;
    }
    Language::from_path(uri.path())
}

/// `getName` → `name`, `setEmail` → `email` / `isEmail`, `isActive` → `isActive`.
/// `None` for anything that isn't shaped like a JavaBean accessor (`get`,
/// `getaway`, `settle`, `island`).
pub(crate) fn parse_java_accessor(method_name: &str) -> Option<JavaAccessor> {
    if let Some(rest) = accessor_suffix(method_name, "get") {
        return Some(JavaAccessor {
            property_names: vec![decapitalize(rest)],
            access: PropertyAccess::Read,
        });
    }
    if accessor_suffix(method_name, "is").is_some() {
        return Some(JavaAccessor {
            property_names: vec![method_name.to_owned()],
            access: PropertyAccess::Read,
        });
    }
    if let Some(rest) = accessor_suffix(method_name, "set") {
        return Some(JavaAccessor {
            property_names: vec![decapitalize(rest), format!("is{rest}")],
            access: PropertyAccess::Write,
        });
    }
    None
}

/// The Java accessor a Kotlin property `property_name` compiles to for `access`.
/// `name` → `getName` / `setName`; `isActive` → `isActive` / `setActive`.
pub(crate) fn java_accessor_name(property_name: &str, access: PropertyAccess) -> String {
    let boolean_suffix = accessor_suffix(property_name, "is");
    match (access, boolean_suffix) {
        (PropertyAccess::Read, Some(_)) => property_name.to_owned(),
        (PropertyAccess::Write, Some(rest)) => format!("set{rest}"),
        (PropertyAccess::Read, None) => format!("get{}", capitalize(property_name)),
        (PropertyAccess::Write, None) => format!("set{}", capitalize(property_name)),
    }
}

/// The Kotlin synthetic property a Java accessor is exposed as, together
/// with the access it serves: `getBalance` → (`balance`, Read),
/// `setBalance` → (`balance`, Write), `isLocked` → (`isLocked`, Read).
pub(crate) fn kotlin_synthetic_property(method_name: &str) -> Option<(String, PropertyAccess)> {
    let accessor = parse_java_accessor(method_name)?;
    let property = accessor.property_names.into_iter().next()?;
    Some((property, accessor.access))
}

/// `prefix` + an uppercase letter, returning what follows `prefix`.
fn accessor_suffix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(prefix)?;
    let first = rest.chars().next()?;
    first.is_uppercase().then_some(rest)
}

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// JavaBean decapitalization: `Name` → `name`, but `URL` → `URL` (a run of
/// capitals stays as written, matching `java.beans.Introspector`).
fn decapitalize(name: &str) -> String {
    let mut chars = name.chars();
    let first = chars.next();
    let second = chars.next();
    match (first, second) {
        (Some(first), Some(second)) if first.is_uppercase() && second.is_uppercase() => {
            name.to_owned()
        }
        (Some(first), _) => first.to_lowercase().chain(name.chars().skip(1)).collect(),
        (None, _) => String::new(),
    }
}

/// Whether `symbol` is a Kotlin property (a `val`/`var`, member or
/// constructor parameter) that Java reaches through accessors. A
/// `const val` or `@JvmField` property is a plain field to Java instead,
/// read by its own name.
pub(crate) fn is_kotlin_property_with_accessors(symbol: &SymbolEntry, lines: &[String]) -> bool {
    let is_property = matches!(
        symbol.kind,
        SymbolKind::PROPERTY | SymbolKind::FIELD | SymbolKind::VARIABLE
    );
    let line = symbol.selection_range.start.line as usize;
    let is_jvm_field = crate::parser::annotation_lines_at(lines, line)
        .into_iter()
        .any(|annotation_line| annotation_line.contains("@JvmField"));
    is_property && !is_jvm_field
}

/// Whether a Kotlin property is a `var`: the indexer records `var` as
/// `VARIABLE` and `val` as `PROPERTY`.
pub(crate) fn is_mutable_property(symbol: &SymbolEntry) -> bool {
    symbol.kind == SymbolKind::VARIABLE
}

/// The `@JvmName("…")` a declaration at `line` carries, if any. Reads the
/// declaration's own line and the annotation lines directly above it.
pub(crate) fn jvm_name_at_line(lines: &[String], line: usize) -> Option<String> {
    crate::parser::annotation_lines_at(lines, line)
        .into_iter()
        .find_map(|annotation_line| jvm_name_argument(annotation_line, "@JvmName("))
}

/// The JVM class name top-level declarations of a Kotlin file compile into:
/// `@file:JvmName("TextHelper")` when present, otherwise the file name with
/// `Kt` appended (`StringUtils.kt` → `StringUtilsKt`).
pub(crate) fn file_facade_class_name(file_uri: &Url, lines: &[String]) -> Option<String> {
    let explicit = lines
        .iter()
        .take_while(|line| !line.trim_start().starts_with("package "))
        .find_map(|line| jvm_name_argument(line, "@file:JvmName("));
    if explicit.is_some() {
        return explicit;
    }
    let path = file_uri.path();
    let file_name = path.rsplit('/').next()?;
    let stem = file_name.strip_suffix(".kt")?;
    Some(format!("{}Kt", capitalize(stem)))
}

/// The string argument of `marker` (e.g. `@JvmName(`) on `line`:
/// `@JvmName("removeAll")` → `removeAll`.
fn jvm_name_argument(line: &str, marker: &str) -> Option<String> {
    let start = line.find(marker)? + marker.len();
    let argument = line[start..].trim_start();
    let quoted = argument.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let name = &quoted[..end];
    (!name.is_empty()).then(|| name.to_owned())
}

// ─── index-backed lookups ─────────────────────────────────────────────────────

/// The declaration a resolved `Location` points at, with the file that
/// declares it. `None` when the file isn't readable or no symbol's name
/// span matches the location exactly.
pub(crate) struct DeclarationAt {
    pub(crate) file: Arc<FileData>,
    symbol_index: usize,
}

impl DeclarationAt {
    pub(crate) fn find(indexer: &Indexer, location: &Location) -> Option<Self> {
        let file = ensure_file_data(indexer, &location.uri)?;
        let symbol_index = file
            .symbols
            .iter()
            .position(|symbol| symbol.selection_range == location.range)?;
        Some(DeclarationAt { file, symbol_index })
    }

    pub(crate) fn symbol(&self) -> &SymbolEntry {
        &self.file.symbols[self.symbol_index]
    }
}

/// Kotlin can't see a Java member that is `private` to its class — a Kotlin
/// file is never inside a Java class body. Such a member must not shadow the
/// accessor Kotlin really reaches (`account.balance` → `getBalance()`, not
/// the `private long balance` field).
pub(crate) fn retain_visible_to_kotlin(
    indexer: &Indexer,
    locations: Vec<Location>,
) -> Vec<Location> {
    locations
        .into_iter()
        .filter(|location| {
            if language_of(&location.uri) != Language::Java {
                return true;
            }
            let Some(declaration) = DeclarationAt::find(indexer, location) else {
                return true;
            };
            declaration.symbol().visibility != Visibility::Private
        })
        .collect()
}

/// Keep the candidates that are Kotlin properties Java can reach with
/// `access`: any property for a read, only a `var` for a write.
pub(crate) fn retain_kotlin_properties_for(
    indexer: &Indexer,
    locations: Vec<Location>,
    access: PropertyAccess,
) -> Vec<Location> {
    locations
        .into_iter()
        .filter(|location| {
            if language_of(&location.uri) != Language::Kotlin {
                return false;
            }
            let Some(declaration) = DeclarationAt::find(indexer, location) else {
                return false;
            };
            let symbol = declaration.symbol();
            if !is_kotlin_property_with_accessors(symbol, &declaration.file.lines) {
                return false;
            }
            match access {
                PropertyAccess::Read => true,
                PropertyAccess::Write => is_mutable_property(symbol),
            }
        })
        .collect()
}

/// Members of the Kotlin class/object declared at `container` that Java
/// calls `java_name` because of an `@JvmName("java_name")` annotation.
pub(crate) fn members_with_jvm_name(
    indexer: &Indexer,
    container: &Location,
    java_name: &str,
) -> Vec<Location> {
    if language_of(&container.uri) != Language::Kotlin {
        return vec![];
    }
    let Some(declaration) = DeclarationAt::find(indexer, container) else {
        return vec![];
    };
    let container_range = declaration.symbol().range;
    declaration
        .file
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.range != container_range && range_encloses(container_range, symbol.range)
        })
        .filter(|symbol| {
            let line = symbol.selection_range.start.line as usize;
            jvm_name_at_line(&declaration.file.lines, line).as_deref() == Some(java_name)
        })
        .map(|symbol| Location {
            uri: container.uri.clone(),
            range: symbol.selection_range,
        })
        .collect()
}

/// Whether the declaration at `location` is a Kotlin `object` — the only
/// kind Java reaches through the `INSTANCE` field.
pub(crate) fn is_kotlin_object(indexer: &Indexer, location: &Location) -> bool {
    if language_of(&location.uri) != Language::Kotlin {
        return false;
    }
    DeclarationAt::find(indexer, location)
        .is_some_and(|declaration| declaration.symbol().kind == SymbolKind::OBJECT)
}

/// Java's `StringUtilsKt` / `TextHelper` (a `@file:JvmName`) — the facade
/// class a Kotlin file's top-level declarations compile into — resolved to
/// the start of that Kotlin file. The facade's package comes from the Java
/// file's own import of it, or is the Java file's own package.
pub(crate) fn resolve_file_facade(
    indexer: &Indexer,
    facade_name: &str,
    from_uri: &Url,
) -> Vec<Location> {
    let Some(from_file) = ensure_file_data(indexer, from_uri) else {
        return vec![];
    };
    let imported_package = from_file.imports.iter().find_map(|import| {
        let is_facade_import = !import.is_star && import.local_name == facade_name;
        is_facade_import
            .then(|| import.full_path.rsplit_once('.'))
            .flatten()
            .map(|(package, _)| package.to_owned())
    });
    let Some(package) = imported_package.or_else(|| from_file.package.clone()) else {
        return vec![];
    };
    let file_ids = match indexer.packages.get(&package) {
        Some(file_ids) => file_ids.clone(),
        None => return vec![],
    };
    file_ids
        .into_iter()
        .filter_map(|file_id| indexer.file_table.url(file_id))
        .filter(|uri| language_of(uri) == Language::Kotlin)
        .filter(|uri| {
            ensure_file_data(indexer, uri).is_some_and(|file| {
                file_facade_class_name(uri, &file.lines).as_deref() == Some(facade_name)
            })
        })
        .map(|uri| Location {
            uri: (*uri).clone(),
            range: Default::default(),
        })
        .collect()
}

// ─── find-references across the language boundary ─────────────────────────────

/// A spelling of a declaration that only code in `language` uses — what
/// find-references must also search for to see that language's usages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InteropAlias {
    pub(crate) name: String,
    pub(crate) language: Language,
}

/// The declaration(s) a find-references request is about, and the other
/// language's spellings of them.
pub(crate) struct InteropReferenceTarget {
    declarations: Vec<Location>,
    pub(crate) aliases: Vec<InteropAlias>,
}

impl InteropReferenceTarget {
    /// The target under `position`: the declaration itself when the cursor
    /// is on one, otherwise whatever the usage under the cursor resolves to.
    /// `None` when nothing resolves or no declaration has a cross-language
    /// spelling.
    pub(crate) fn at(indexer: &Indexer, uri: &Url, position: Position) -> Option<Self> {
        let cursor_word = indexer
            .word_and_qualifier_at(uri, position)
            .map(|(word, _)| word)?;
        let declarations = match declaration_under_cursor(indexer, uri, position) {
            Some(declaration) => vec![declaration],
            None => definitions_at(indexer, uri, position, LookupIo::Full),
        };
        let mut aliases: Vec<InteropAlias> = Vec::new();
        for location in &declarations {
            let Some(declaration) = DeclarationAt::find(indexer, location) else {
                continue;
            };
            let own_name = InteropAlias {
                name: declaration.symbol().name.clone(),
                language: language_of(&location.uri),
            };
            // Asked from a usage spelled the other way (`getName()` for
            // `val name`): the declaration's own spelling still needs a search.
            let spellings = aliases_of(&location.uri, &declaration)
                .into_iter()
                .chain((own_name.name != cursor_word).then_some(own_name));
            for alias in spellings {
                if !aliases.contains(&alias) {
                    aliases.push(alias);
                }
            }
        }
        (!aliases.is_empty()).then_some(InteropReferenceTarget {
            declarations,
            aliases,
        })
    }

    /// Whether the occurrence at `location` is a usage of this target: it
    /// goes to one of the target's declarations, exactly as go-to-definition
    /// from that spot would. The resolver's own interop rules (getter vs.
    /// setter for a read vs. a write, receiver type, `INSTANCE`, facades)
    /// are what accept or reject it.
    pub(crate) fn is_reached_from(&self, indexer: &Indexer, location: &Location) -> bool {
        let is_declaration = self.declarations.iter().any(|declaration| {
            declaration.uri == location.uri && declaration.range == location.range
        });
        if is_declaration {
            return false;
        }
        definitions_at(
            indexer,
            &location.uri,
            location.range.start,
            LookupIo::IndexOnly,
        )
        .iter()
        .any(|definition| {
            self.declarations.iter().any(|declaration| {
                declaration.uri == definition.uri
                    && declaration.range.start == definition.range.start
            })
        })
    }
}

/// The other language's spellings of one declaration — see the module docs.
fn aliases_of(uri: &Url, declaration: &DeclarationAt) -> Vec<InteropAlias> {
    let symbol = declaration.symbol();
    let lines = &declaration.file.lines;
    let java_spelling = |name: String| InteropAlias {
        name,
        language: Language::Java,
    };
    match language_of(uri) {
        Language::Kotlin => {
            let line = symbol.selection_range.start.line as usize;
            if let Some(jvm_name) = jvm_name_at_line(lines, line) {
                return vec![java_spelling(jvm_name)];
            }
            if is_kotlin_property_with_accessors(symbol, lines) {
                let mut accessors = vec![java_spelling(java_accessor_name(
                    &symbol.name,
                    PropertyAccess::Read,
                ))];
                if is_mutable_property(symbol) {
                    accessors.push(java_spelling(java_accessor_name(
                        &symbol.name,
                        PropertyAccess::Write,
                    )));
                }
                return accessors;
            }
            // Same spelling in Java, but reached through a facade class or
            // an import the Kotlin-oriented scoping doesn't recognise.
            vec![java_spelling(symbol.name.clone())]
        }
        Language::Java => {
            let is_method = symbol.kind == SymbolKind::METHOD;
            let synthetic_property = is_method
                .then(|| kotlin_synthetic_property(&symbol.name))
                .flatten();
            synthetic_property
                .map(|(property_name, _)| InteropAlias {
                    name: property_name,
                    language: Language::Kotlin,
                })
                .into_iter()
                .collect()
        }
        Language::Swift => vec![],
    }
}

/// The indexed declaration whose name the cursor is on, if any.
fn declaration_under_cursor(indexer: &Indexer, uri: &Url, position: Position) -> Option<Location> {
    let file = ensure_file_data(indexer, uri)?;
    let symbol = file.symbols.iter().find(|symbol| {
        let range = symbol.selection_range;
        range.start.line == position.line
            && range.start.character <= position.character
            && position.character <= range.end.character
    })?;
    Some(Location {
        uri: uri.clone(),
        range: symbol.selection_range,
    })
}

/// How much work [`definitions_at`] may do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LookupIo {
    /// Go-to-definition's full chain, `rg`/`fd` fallbacks included — for the
    /// one cursor position a request is about.
    Full,
    /// Index only, never a subprocess — for verifying each of possibly
    /// hundreds of `rg` hits.
    IndexOnly,
}

/// Where go-to-definition from `position` lands: the CST-resolved identity
/// when the CST can type the receiver, otherwise the qualified/bare lookup.
fn definitions_at(indexer: &Indexer, uri: &Url, position: Position, io: LookupIo) -> Vec<Location> {
    let index_only = io == LookupIo::IndexOnly;
    if let Some(symbol) = crate::indexer::classify_cursor(indexer, uri, position) {
        if let crate::indexer::NavigationSource::CstResolved(definitions) =
            crate::indexer::resolve_identity_with_io(&symbol, indexer, uri, index_only)
        {
            if !definitions.is_empty() {
                return definitions.0;
            }
        }
    }
    let Some((word, qualifier)) = indexer.word_and_qualifier_at(uri, position) else {
        return vec![];
    };
    match io {
        LookupIo::Full => indexer.find_definition_qualified(&word, qualifier.as_deref(), uri),
        LookupIo::IndexOnly => {
            indexer.find_definition_qualified_index_only(&word, qualifier.as_deref(), uri)
        }
    }
}

#[cfg(test)]
#[path = "jvm_interop_tests.rs"]
mod tests;

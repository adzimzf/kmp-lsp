//! What a find-references request is about, and how to tell whether an
//! occurrence somewhere else is a usage of it.
//!
//! [`ReferenceTarget`] answers both for the two searches the `rg` pass can't
//! do on its own:
//!
//! - **Other spellings in the workspace** — Java's `getName()` for Kotlin's
//!   `val name` and the like (see [`super::jvm_interop`]).
//! - **Usages inside dependency sources** — the `*-sources.jar` files the
//!   JAR pipeline mounts in memory, which `rg` can't search inside.
//!
//! Every candidate occurrence is accepted only when go-to-definition from it
//! lands on the target's declaration, so the resolver's own rules (receiver
//! type, getter vs. setter, `INSTANCE`, facades, same-named decoys) decide.

use tower_lsp::lsp_types::{Location, Position, Range, Url};

use crate::indexer::Indexer;
use crate::types::{FileData, Language, SourceSet};

use super::jvm_interop::{aliases_of, language_of, DeclarationAt};
use super::resolve::ensure_file_data;

/// Library occurrences verified per request at most — each one costs a
/// resolution, and a name like `get` can occur hundreds of thousands of
/// times across all mounted sources.
const MAX_LIBRARY_CANDIDATES: usize = 5_000;

/// A name a declaration is written as, and which language's files write it
/// that way (`None`: any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spelling {
    pub(crate) name: String,
    pub(crate) language: Option<Language>,
}

impl Spelling {
    pub(crate) fn is_written_in(&self, uri: &Url) -> bool {
        self.language
            .is_none_or(|language| language_of(uri) == language)
    }
}

/// Where a declaration lives, for narrowing which library files could
/// possibly use it.
struct DeclarationScope {
    package: Option<String>,
    container: Option<String>,
}

impl DeclarationScope {
    /// Whether `file` can see the declaration at all: same package, an
    /// import from its package, or a mention of its declaring class (a
    /// member reached through an inferred receiver).
    fn may_be_used_in(&self, file: &FileData) -> bool {
        let Some(package) = self.package.as_deref() else {
            return true;
        };
        if file.package.as_deref() == Some(package) {
            return true;
        }
        let package_prefix = format!("{package}.");
        let imports_from_package = file.imports.iter().any(|import| {
            import.full_path == package || import.full_path.starts_with(&package_prefix)
        });
        if imports_from_package {
            return true;
        }
        self.container.as_deref().is_some_and(|container| {
            file.lines
                .iter()
                .any(|line| !word_columns(line, container).is_empty())
        })
    }
}

/// The declaration(s) a find-references request is about.
pub(crate) struct ReferenceTarget {
    declarations: Vec<Location>,
    scopes: Vec<DeclarationScope>,
    /// Every spelling of the declarations: their own names plus the other
    /// language's (`getName` for `val name`, a `@JvmName`, …).
    spellings: Vec<Spelling>,
    /// The word under the cursor — the `rg` pass already searched it.
    cursor_word: String,
}

impl ReferenceTarget {
    /// The target under `position`: the declaration itself when the cursor
    /// is on one, otherwise whatever the usage under the cursor resolves to.
    /// A cursor in an extracted library file is mapped back to the indexed
    /// `jar:` entry it came from. `None` when nothing resolves.
    pub(crate) fn at(indexer: &Indexer, uri: &Url, position: Position) -> Option<Self> {
        let uri = indexer
            .original_jar_source_uri(uri)
            .unwrap_or_else(|| uri.clone());
        let cursor_word = indexer
            .word_and_qualifier_at(&uri, position)
            .map(|(word, _)| word)?;
        let declarations = match declaration_under_cursor(indexer, &uri, position) {
            Some(declaration) => vec![declaration],
            None => definitions_at(indexer, &uri, position, LookupIo::Full),
        };
        let mut scopes = Vec::new();
        let mut spellings: Vec<Spelling> = Vec::new();
        for location in &declarations {
            let Some(declaration) = DeclarationAt::find(indexer, location) else {
                continue;
            };
            scopes.push(DeclarationScope {
                package: declaration.file.package.clone(),
                container: declaration.symbol().container.clone(),
            });
            let own_name = Spelling {
                name: declaration.symbol().name.clone(),
                language: None,
            };
            for spelling in std::iter::once(own_name).chain(aliases_of(&location.uri, &declaration))
            {
                if !spellings.contains(&spelling) {
                    spellings.push(spelling);
                }
            }
        }
        if spellings.is_empty() {
            return None;
        }
        Some(ReferenceTarget {
            declarations,
            scopes,
            spellings,
            cursor_word,
        })
    }

    /// The spellings the workspace `rg` pass didn't already cover: all but
    /// the any-language spelling of the word under the cursor. A
    /// language-specific spelling of the same word stays — Java's
    /// `StringUtilsKt.slugify(…)` uses the Kotlin name, but the `rg` pass's
    /// Kotlin-style import scoping never reaches that Java file.
    pub(crate) fn other_spellings(&self) -> Vec<Spelling> {
        self.spellings
            .iter()
            .filter(|spelling| spelling.name != self.cursor_word || spelling.language.is_some())
            .cloned()
            .collect()
    }

    /// Whether the occurrence at `location` is a usage of this target: it
    /// goes to one of the target's declarations, exactly as go-to-definition
    /// from that spot would. The resolver's own rules (receiver type, getter
    /// vs. setter for a read vs. a write, `INSTANCE`, facades) accept or
    /// reject it.
    pub(crate) fn is_reached_from(&self, indexer: &Indexer, location: &Location) -> bool {
        let is_declaration = self.declarations.iter().any(|declaration| {
            declaration.uri == location.uri && declaration.range == location.range
        });
        if is_declaration || !is_code_identifier_at(indexer, location) {
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

    /// Usages inside library sources (mounted `*-sources.jar` entries and
    /// `sourcePaths` libraries), as `jar:`/`file:` locations of the indexed
    /// copies. Only files that can see the declaration are searched, and
    /// only verified occurrences are returned.
    pub(crate) fn library_references(&self, indexer: &Indexer) -> Vec<Location> {
        self.load_libraries_that_may_use_it(indexer);
        // Collected before verifying: resolution reads `files` again, and
        // must not run while this iteration holds its shard guards.
        let mut candidates: Vec<Location> = Vec::new();
        for entry in indexer.files.iter() {
            if candidates.len() >= MAX_LIBRARY_CANDIDATES {
                log::info!(
                    "references: library search stopped at {MAX_LIBRARY_CANDIDATES} candidate occurrences"
                );
                break;
            }
            let file = entry.value();
            if file.source_set != SourceSet::Library {
                continue;
            }
            let Ok(uri) = Url::parse(entry.key()) else {
                continue;
            };
            if !self.scopes.iter().any(|scope| scope.may_be_used_in(file)) {
                continue;
            }
            for spelling in self
                .spellings
                .iter()
                .filter(|spelling| spelling.is_written_in(&uri))
            {
                candidates.extend(occurrences(&uri, &file.lines, &spelling.name));
            }
        }
        candidates
            .into_iter()
            .filter(|location| self.is_reached_from(indexer, location))
            .collect()
    }
}

impl ReferenceTarget {
    /// Libraries are loaded on demand (see
    /// [`crate::indexer::library_sources`]): load the ones whose text
    /// mentions a spelling and something every user of the declaration has
    /// to mention too — its package's last segment (in an import or the
    /// package line) or its class.
    fn load_libraries_that_may_use_it(&self, indexer: &Indexer) {
        let spellings: Vec<&str> = self
            .spellings
            .iter()
            .map(|spelling| spelling.name.as_str())
            .collect();
        let unscoped =
            self.scopes.is_empty() || self.scopes.iter().any(|scope| scope.package.is_none());
        let mut context: Vec<&str> = Vec::new();
        if !unscoped {
            for scope in &self.scopes {
                if let Some(segment) = scope
                    .package
                    .as_deref()
                    .and_then(|package| package.rsplit('.').next())
                {
                    context.push(segment);
                }
                if let Some(class) = scope
                    .container
                    .as_deref()
                    .and_then(|container| container.rsplit('.').next())
                {
                    context.push(class);
                }
            }
        }
        let loaded = crate::indexer::library_sources::load_libraries_mentioning(
            indexer, &spellings, &context,
        );
        if loaded > 0 {
            log::debug!("references: loaded {loaded} libraries that mention {spellings:?}");
        }
    }
}

/// Whether `location` starts on an identifier in the parse tree — not inside
/// a comment or a string, where a text search also finds names (Javadoc's
/// `{@link #toJson(Object)}`, a code sample in a KDoc block).
fn is_code_identifier_at(indexer: &Indexer, location: &Location) -> bool {
    let Some(doc) = indexer.live_doc_or_parse(&location.uri) else {
        return false;
    };
    let cursor = crate::types::CursorPos {
        line: location.range.start.line as usize,
        utf16_col: location.range.start.character as usize,
    };
    crate::indexer::cursor_node_at(&doc, cursor).is_some_and(|node| {
        matches!(
            node.kind(),
            crate::queries::KIND_SIMPLE_IDENT
                | crate::queries::KIND_IDENTIFIER
                | crate::queries::KIND_TYPE_IDENT
        )
    })
}

/// Every whole-word occurrence of `word` in `lines`.
fn occurrences(uri: &Url, lines: &[String], word: &str) -> Vec<Location> {
    let mut found = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        for column in word_columns(line, word) {
            let start = Position {
                line: line_index as u32,
                character: column,
            };
            let end = Position {
                line: line_index as u32,
                character: column + word.encode_utf16().count() as u32,
            };
            found.push(Location {
                uri: uri.clone(),
                range: Range { start, end },
            });
        }
    }
    found
}

/// UTF-16 columns where `word` occurs in `line` as a whole identifier.
fn word_columns(line: &str, word: &str) -> Vec<u32> {
    if word.is_empty() {
        return Vec::new();
    }
    let is_identifier_char = |character: char| character.is_alphanumeric() || character == '_';
    line.match_indices(word)
        .filter(|(start, _)| {
            let before = line[..*start].chars().next_back();
            let after = line[start + word.len()..].chars().next();
            !before.is_some_and(is_identifier_char) && !after.is_some_and(is_identifier_char)
        })
        .map(|(start, _)| line[..start].encode_utf16().count() as u32)
        .collect()
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
#[path = "reference_target_tests.rs"]
mod tests;

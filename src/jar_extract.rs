//! Lazy extraction of JAR/sources-JAR entries to disk for go-to-definition.
//!
//! Auto-mounted library sources are indexed in-memory and exposed under
//! `jar:file://…!/Foo.kt` URIs. Hover/completion work from the index, but go-def
//! hands the *editor* a URI to open, and editors only open `file://`. So when a
//! go-def target is a `jar:` entry, we extract just that one zip entry to a
//! read-only cache file and return a `file://` Location (same range) the editor
//! can actually navigate into.
//!
//! A compiled-only JAR (no sources jar — e.g. an internal library that never
//! published one) has no source at all; its URI carries no `!/entry`. For
//! those, the signatures the sidecar read from the classes (one per line,
//! the same synthetic line numbers the index uses) are written to a
//! read-only `.kt` stub, so go-to-definition still opens something — like
//! an IDE's "decompiled" view, but signatures only.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, LocationLink, Position, Range, Url};

use crate::indexer::Indexer;

/// Parse a `jar:file://<jar>!/<entry>` URI into `(jar_path, entry)`.
/// Returns `None` for non-`jar:` URIs and for compiled-only JAR URIs that carry
/// no `!/entry` (nothing to extract).
pub(crate) fn parse_jar_entry_uri(uri: &str) -> Option<(PathBuf, String)> {
    let rest = uri.strip_prefix("jar:")?;
    let (jar_part, entry) = rest.split_once("!/")?;
    if entry.is_empty() || is_unsafe_entry(entry) {
        return None;
    }
    // Reuse `Url::to_file_path` so percent-encoding in the path is decoded.
    let jar_path = Url::parse(jar_part).ok()?.to_file_path().ok()?;
    Some((jar_path, entry.to_owned()))
}

/// Directory under which jar entries are extracted (`~/.cache/kmp-lsp/jar-sources`).
fn jar_sources_cache_dir() -> PathBuf {
    crate::indexer::xdg_cache_base()
        .join("kmp-lsp")
        .join("jar-sources")
}

/// Whether `uri` points at a file we extracted from a jar. Such files must not be
/// re-indexed as workspace documents when the editor opens them — their symbols are
/// already in the index under the original `jar:` URI, so indexing the extracted
/// `file://` copy too would duplicate every library definition.
pub(crate) fn is_extracted_jar_source(uri: &Url) -> bool {
    uri.to_file_path()
        .map(|path| path.starts_with(jar_sources_cache_dir()))
        .unwrap_or(false)
}

/// Reject entry paths that could escape the extraction cache dir when joined:
/// absolute paths, Windows drive letters, and any `..` traversal segment.
fn is_unsafe_entry(entry: &str) -> bool {
    entry.starts_with('/')
        || entry.starts_with('\\')
        || (entry.len() >= 2 && entry.as_bytes()[1] == b':') // C:\…
        || entry.split(['/', '\\']).any(|segment| segment == "..")
}

/// Extract `entry` from `jar_path` into the on-disk cache and return a `file://`
/// `Url` for the extracted (read-only) file. Idempotent: a file already present
/// for the jar's current `(mtime, size)` is reused. Returns `None` on any
/// I/O / zip error or if the entry is absent.
pub(crate) fn extract_jar_entry_to_disk(jar_path: &Path, entry: &str) -> Option<Url> {
    // Defensive: never let an entry path escape the cache dir, even if a caller
    // passes something other than a zip-validated name.
    if entry.is_empty() || is_unsafe_entry(entry) {
        return None;
    }
    let metadata = std::fs::metadata(jar_path).ok()?;
    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut hasher = DefaultHasher::new();
    jar_path.hash(&mut hasher);
    mtime_secs.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    let fingerprint = hasher.finish();

    let stem = jar_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("jar");
    let dest = jar_sources_cache_dir()
        .join(format!("{stem}-{fingerprint:016x}"))
        .join(entry);

    if dest.is_file() {
        return Url::from_file_path(&dest).ok();
    }

    let file = std::fs::File::open(jar_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut zip_entry = archive.by_name(entry).ok()?;
    let mut bytes = Vec::new();
    zip_entry.read_to_end(&mut bytes).ok()?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::write(&dest, &bytes).ok()?;
    set_read_only(&dest);
    Url::from_file_path(&dest).ok()
}

/// Rewrite any `jar:…!/entry` definition target to a `file://` one backed by an
/// extracted on-disk file. Non-`jar:` / unextractable targets pass through.
pub(crate) fn rewrite_jar_definitions(
    indexer: &Indexer,
    response: GotoDefinitionResponse,
) -> GotoDefinitionResponse {
    let response = prefer_sources_over_compiled(response);
    match response {
        GotoDefinitionResponse::Scalar(location) => {
            GotoDefinitionResponse::Scalar(rewrite_location(indexer, location))
        }
        GotoDefinitionResponse::Array(locations) => GotoDefinitionResponse::Array(
            locations
                .into_iter()
                .map(|location| rewrite_location(indexer, location))
                .collect(),
        ),
        GotoDefinitionResponse::Link(links) => GotoDefinitionResponse::Link(
            links
                .into_iter()
                .map(|link| rewrite_link(indexer, link))
                .collect(),
        ),
    }
}

/// [`rewrite_jar_definitions`] for a plain list of locations (find-references
/// results that land in library sources).
pub(crate) fn rewrite_jar_locations(indexer: &Indexer, locations: Vec<Location>) -> Vec<Location> {
    locations
        .into_iter()
        .map(|location| rewrite_location(indexer, location))
        .collect()
}

fn rewrite_location(indexer: &Indexer, location: Location) -> Location {
    if let Some(file_uri) = rewrite_jar_uri(indexer, &location.uri) {
        return Location {
            uri: file_uri,
            range: location.range,
        };
    }
    match compiled_jar_stub(indexer, &location.uri) {
        Some(stub) => Location {
            range: stub.name_range(location.range),
            uri: stub.uri,
        },
        None => location,
    }
}

fn rewrite_link(indexer: &Indexer, mut link: LocationLink) -> LocationLink {
    if let Some(file_uri) = rewrite_jar_uri(indexer, &link.target_uri) {
        link.target_uri = file_uri;
    } else if let Some(stub) = compiled_jar_stub(indexer, &link.target_uri) {
        link.target_range = stub.name_range(link.target_range);
        link.target_selection_range = stub.name_range(link.target_selection_range);
        link.target_uri = stub.uri;
    }
    link
}

/// A compiled-only JAR location (`jar:file://…/x.jar`, no `!/entry`).
fn is_compiled_only_jar(uri: &Url) -> bool {
    uri.scheme() == "jar" && !uri.as_str().contains("!/")
}

/// When the same lookup found real source (a sources-jar entry or a file)
/// and compiled-only JAR matches, keep the source: the compiled match is a
/// signature-only fallback for libraries that publish no sources.
fn prefer_sources_over_compiled(response: GotoDefinitionResponse) -> GotoDefinitionResponse {
    match response {
        GotoDefinitionResponse::Array(locations) => {
            let has_source = locations
                .iter()
                .any(|location| !is_compiled_only_jar(&location.uri));
            if !has_source {
                return GotoDefinitionResponse::Array(locations);
            }
            let mut kept: Vec<Location> = locations
                .into_iter()
                .filter(|location| !is_compiled_only_jar(&location.uri))
                .collect();
            if kept.len() == 1 {
                GotoDefinitionResponse::Scalar(kept.remove(0))
            } else {
                GotoDefinitionResponse::Array(kept)
            }
        }
        GotoDefinitionResponse::Link(links) => {
            let has_source = links
                .iter()
                .any(|link| !is_compiled_only_jar(&link.target_uri));
            if !has_source {
                return GotoDefinitionResponse::Link(links);
            }
            GotoDefinitionResponse::Link(
                links
                    .into_iter()
                    .filter(|link| !is_compiled_only_jar(&link.target_uri))
                    .collect(),
            )
        }
        scalar @ GotoDefinitionResponse::Scalar(_) => scalar,
    }
}

/// The signature stub written for a compiled-only JAR.
struct CompiledJarStub {
    uri: Url,
    lines: Vec<String>,
}

impl CompiledJarStub {
    /// The index's synthetic range is `(line, 0..name length)`; point it at
    /// the symbol's name within that line's signature instead.
    fn name_range(&self, synthetic: Range) -> Range {
        let line = synthetic.start.line;
        let name_length = synthetic
            .end
            .character
            .saturating_sub(synthetic.start.character);
        let Some(text) = self.lines.get(line as usize) else {
            return synthetic;
        };
        let name = text
            .split(|character: char| {
                !(character.is_alphanumeric() || character == '_' || character == '$')
            })
            .find(|word| word.chars().count() as u32 == name_length && !word.is_empty());
        let column = name.and_then(|name| {
            // The declared name is the word after the keyword(s) and any
            // receiver: the last occurrence before `(`, `:`, `<` or the end.
            let signature_end = text.find(['(', ':', '=']).unwrap_or(text.len());
            text[..signature_end].rfind(name)
        });
        match column {
            Some(byte_column) => {
                let character = text[..byte_column].encode_utf16().count() as u32;
                Range {
                    start: Position { line, character },
                    end: Position {
                        line,
                        character: character + name_length,
                    },
                }
            }
            None => synthetic,
        }
    }
}

/// Write (once per jar version) and return the signature stub for a
/// compiled-only `jar:file://…/x.jar` URI the sidecar indexed. `None` for
/// any other URI.
fn compiled_jar_stub(indexer: &Indexer, uri: &Url) -> Option<CompiledJarStub> {
    if !is_compiled_only_jar(uri) {
        return None;
    }
    let file_data = indexer.jar_files.get(uri.as_str())?.value().clone();
    let packages = indexer
        .jar_symbol_packages
        .get(uri.as_str())
        .map(|packages| packages.value().clone())
        .unwrap_or_default();
    let jar_path = Url::parse(uri.as_str().strip_prefix("jar:")?)
        .ok()?
        .to_file_path()
        .ok()?;
    let lines = stub_lines(&file_data, &packages);
    let stem = jar_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("jar");
    let destination = jar_sources_cache_dir()
        .join("compiled-stubs")
        .join(format!("{stem}-{:016x}.kt", jar_fingerprint(&jar_path)?));
    if !destination.is_file() {
        let mut content = lines.join("\n");
        content.push_str(&format!(
            "\n\n// Signatures read from the compiled classes of {} — the library\n\
             // publishes no sources jar, so there are no bodies or comments to show.\n",
            jar_path.display()
        ));
        let parent = destination.parent()?;
        std::fs::create_dir_all(parent).ok()?;
        // Write-then-rename: a concurrent request never sees a partial file.
        let temporary = destination.with_extension("kt.tmp");
        std::fs::write(&temporary, content).ok()?;
        std::fs::rename(&temporary, &destination).ok()?;
        set_read_only(&destination);
    }
    let file_uri = Url::from_file_path(&destination).ok()?;
    indexer.library_uris.insert(file_uri.as_str().to_owned());
    Some(CompiledJarStub {
        uri: file_uri,
        lines,
    })
}

/// One line per indexed symbol, at the symbol's own synthetic line: its
/// signature, then where it's declared.
fn stub_lines(file_data: &crate::types::FileData, packages: &[String]) -> Vec<String> {
    file_data
        .lines
        .iter()
        .enumerate()
        .map(|(index, signature)| {
            let symbol = file_data
                .symbols
                .iter()
                .find(|symbol| symbol.selection_range.start.line as usize == index);
            let package = packages.get(index).filter(|package| !package.is_empty());
            let container = symbol.and_then(|symbol| symbol.container.as_deref());
            let declared_in = match (package, container) {
                (Some(package), Some(container)) => format!("{package}.{container}"),
                (Some(package), None) => package.clone(),
                (None, Some(container)) => container.to_owned(),
                (None, None) => String::new(),
            };
            if declared_in.is_empty() {
                signature.clone()
            } else {
                format!("{signature}    // {declared_in}")
            }
        })
        .collect()
}

/// `(path, mtime, size)` hash — a new jar version gets a new stub.
pub(crate) fn jar_fingerprint(jar_path: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(jar_path).ok()?;
    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let mut hasher = DefaultHasher::new();
    jar_path.hash(&mut hasher);
    mtime_secs.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    Some(hasher.finish())
}

/// Extract the `jar:` URI's entry and return the `file://` URL, registering it
/// as a library URI so references/indexing treat it as read-only library source.
fn rewrite_jar_uri(indexer: &Indexer, uri: &Url) -> Option<Url> {
    if uri.scheme() != "jar" {
        return None;
    }
    let (jar_path, entry) = parse_jar_entry_uri(uri.as_str())?;
    let file_uri = extract_jar_entry_to_disk(&jar_path, &entry)?;
    indexer.library_uris.insert(file_uri.as_str().to_owned());
    // Remember which `jar:` sources entry this file came from, so find-references
    // invoked here can resolve scope from the indexed `jar:` entry's real package.
    indexer.record_extracted_jar_source(&file_uri, uri);
    Some(file_uri)
}

#[cfg(unix)]
fn set_read_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444));
}

#[cfg(not(unix))]
fn set_read_only(_path: &Path) {}

#[cfg(test)]
#[path = "jar_extract_tests.rs"]
mod tests;

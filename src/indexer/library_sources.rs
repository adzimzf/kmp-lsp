//! Library sources (`*-sources.jar`, decompiled sources) loaded on demand.
//!
//! Parsing every dependency's sources up front and keeping them in memory
//! costs hundreds of MB on a real project, most of it for libraries the user
//! never touches. In [`LibrarySourcesMode::OnDemand`] (the default) each
//! sources jar is parsed once, into two cache files per jar version:
//!
//! - an **index** — the names it declares (bare names, top-level FQNs,
//!   extension receivers) plus a hash of every identifier it mentions. Only
//!   this stays in memory: the names go into the same Tier-1 maps compiled
//!   JARs use (`jar_bare_names`, `jar_qualified`, `jar_extension_receivers`),
//!   so completion, auto-import and every promote-before-read site already
//!   know about them;
//! - the **parse** — the full per-file parse results, read back when one of
//!   the jar's names is first used (go-to-definition, hover, completion on
//!   one of its types, an import in an opened file). That load is the
//!   Tier-2 promotion `jar::promote_candidates_bounded` performs for
//!   compiled JARs, and puts the files in the index exactly as the eager
//!   path does.
//!
//! Find-references uses the identifier hashes to load only the jars that
//! mention the name being searched.
//!
//! [`LibrarySourcesMode::Eager`] (`indexingOptions.librarySources: "eager"`)
//! keeps the previous behavior: everything parsed and loaded at startup.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;

use super::sources_jar_cache::{JarFingerprint, SourcesFileEntry};
use crate::types::{JarId, SourceSet};

/// Bump when the on-disk shape of this module's cache files changes. The
/// directory name also embeds `cache::CACHE_VERSION`, which covers changes
/// to `FileData` (see `sources_jar_cache.rs` for why that coupling matters).
const LIBRARY_CACHE_FORMAT: u32 = 1;

/// At most this many libraries are loaded for one find-references request.
const MAX_JARS_PER_REFERENCES: usize = 64;

/// How library sources are loaded (`indexingOptions.librarySources`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibrarySourcesMode {
    /// Parse once, keep only names in memory, load a library when it's used.
    OnDemand,
    /// Load every library at startup.
    Eager,
}

impl LibrarySourcesMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "onDemand" | "on-demand" | "lazy" => Some(Self::OnDemand),
            "eager" => Some(Self::Eager),
            _ => None,
        }
    }
}

/// One sources jar known to the on-demand loader.
struct LibraryJar {
    /// Sorted, deduplicated hashes of every identifier in the jar.
    words: Arc<[u64]>,
    /// Whether its parse is cached on disk (loading it is then cheap).
    parse_cached: bool,
}

/// The on-demand loader's state, owned by the `Indexer`.
pub(crate) struct LibrarySources {
    eager: AtomicBool,
    jars: DashMap<JarId, LibraryJar>,
    /// Where the cache files live (tests point it at a temporary directory).
    cache_directory: RwLock<PathBuf>,
    /// Serializes loads, so two requests never apply the same jar twice.
    load_lock: Mutex<()>,
}

impl Default for LibrarySources {
    fn default() -> Self {
        Self {
            eager: AtomicBool::new(false),
            jars: DashMap::new(),
            cache_directory: RwLock::new(cache_directory(None)),
            load_lock: Mutex::new(()),
        }
    }
}

impl LibrarySources {
    pub(crate) fn mode(&self) -> LibrarySourcesMode {
        if self.eager.load(Ordering::Relaxed) {
            LibrarySourcesMode::Eager
        } else {
            LibrarySourcesMode::OnDemand
        }
    }

    pub(crate) fn set_mode(&self, mode: LibrarySourcesMode) {
        self.eager
            .store(mode == LibrarySourcesMode::Eager, Ordering::Relaxed);
    }

    /// Whether `jar_id` is a sources jar this loader manages.
    pub(crate) fn manages(&self, jar_id: JarId) -> bool {
        self.jars.contains_key(&jar_id)
    }

    /// Whether loading `jar_id` only reads its cached parse (no parsing).
    pub(crate) fn is_cached(&self, jar_id: JarId) -> bool {
        self.jars
            .get(&jar_id)
            .is_some_and(|library| library.parse_cached)
    }

    fn directory(&self) -> PathBuf {
        self.cache_directory
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

// ── Disk format ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    mtime_secs: u64,
    mtime_nanos: u32,
    file_size: u64,
}

impl From<&JarFingerprint> for Stamp {
    fn from(fingerprint: &JarFingerprint) -> Self {
        Self {
            mtime_secs: fingerprint.mtime_secs,
            mtime_nanos: fingerprint.mtime_nanos,
            file_size: fingerprint.file_size,
        }
    }
}

/// What stays in memory for a library that isn't loaded.
#[derive(Serialize, Deserialize, Default, Debug)]
pub(crate) struct LibraryIndex {
    /// The sources jar this describes (used to prune caches of deleted jars).
    jar: String,
    stamp: Option<Stamp>,
    /// Every name the jar's files would add to `definitions`.
    pub(crate) names: Vec<String>,
    /// `package.Name` of every top-level declaration (auto-import).
    pub(crate) top_level: Vec<String>,
    /// Receiver keys of its extension functions and properties.
    pub(crate) extension_receivers: Vec<String>,
    /// Sorted hashes of every identifier its files mention.
    words: Vec<u64>,
}

#[derive(Serialize, Deserialize)]
struct LibraryParse {
    stamp: Stamp,
    files: Vec<SourcesFileEntry>,
}

/// `~/.cache/kmp-lsp/library-sources-v<format>-c<cache version>/`, or the
/// same under `base`.
fn cache_directory(base: Option<&Path>) -> PathBuf {
    let base = match base {
        Some(directory) => directory.to_owned(),
        None => super::cache::xdg_cache_base().join("kmp-lsp"),
    };
    base.join(format!(
        "library-sources-v{LIBRARY_CACHE_FORMAT}-c{}",
        super::cache::CACHE_VERSION
    ))
}

/// The cache files' shared name for `jar`: its file stem plus a hash of its
/// full path (two versions of a library share a stem).
fn cache_stem(jar: &Path) -> String {
    let stem = jar
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "jar".to_owned());
    format!(
        "{stem}-{:016x}",
        stable_hash(jar.to_string_lossy().as_bytes())
    )
}

fn index_path(directory: &Path, jar: &Path) -> PathBuf {
    directory.join(format!("{}.index", cache_stem(jar)))
}

fn parse_path(directory: &Path, jar: &Path) -> PathBuf {
    directory.join(format!("{}.parse", cache_stem(jar)))
}

/// FNV-1a: stable across Rust releases, unlike `DefaultHasher` — these
/// hashes are stored on disk.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

pub(crate) fn word_hash(word: &str) -> u64 {
    stable_hash(word.as_bytes())
}

fn read_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let file = std::fs::File::open(path).ok()?;
    bincode::deserialize_from(std::io::BufReader::new(file)).ok()
}

/// Write atomically (temp file → rename), so a reader never sees half a file.
fn write_file<T: Serialize>(path: &Path, value: &T) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        log::warn!(
            "library sources: cannot create {}: {error}",
            parent.display()
        );
        return false;
    }
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let written = std::fs::File::create(&temporary)
        .map_err(|error| error.to_string())
        .and_then(|file| {
            bincode::serialize_into(std::io::BufWriter::new(file), value)
                .map_err(|error| error.to_string())
        })
        .and_then(|()| std::fs::rename(&temporary, path).map_err(|error| error.to_string()));
    match written {
        Ok(()) => true,
        Err(error) => {
            log::warn!("library sources: cannot write {}: {error}", path.display());
            let _ = std::fs::remove_file(&temporary);
            false
        }
    }
}

// ── Building the cache ────────────────────────────────────────────────────────

/// Identifiers in `line`: runs of letters, digits, `_` and `$` that don't
/// start with a digit.
fn identifiers(line: &str) -> impl Iterator<Item = &str> {
    line.split(|character: char| {
        !(character.is_alphanumeric() || character == '_' || character == '$')
    })
    .filter(|word| {
        word.chars()
            .next()
            .is_some_and(|first| !first.is_ascii_digit())
    })
}

/// The in-memory index of a parsed jar.
fn index_of(jar: &Path, stamp: Stamp, files: &[SourcesFileEntry]) -> LibraryIndex {
    let mut names: Vec<String> = Vec::new();
    let mut top_level: Vec<String> = Vec::new();
    let mut extension_receivers: Vec<String> = Vec::new();
    let mut words: Vec<u64> = Vec::new();
    for file in files {
        let Ok(uri) = Url::parse(&file.uri) else {
            continue;
        };
        let supertypes = super::apply::derive_supertypes(&uri, &file.file_data);
        let contributions = super::apply::contributions_from_data(
            &uri,
            Arc::clone(&file.file_data),
            file.content_hash,
            &supertypes,
        );
        names.extend(contributions.definitions.into_keys());
        extension_receivers.extend(contributions.extensions.into_keys());
        if let Some(package) = file.file_data.package.as_deref().filter(|p| !p.is_empty()) {
            top_level.extend(
                file.file_data
                    .symbols
                    .iter()
                    .filter(|symbol| symbol.container.is_none())
                    .map(|symbol| format!("{package}.{}", symbol.name)),
            );
        }
        for line in file.file_data.lines.iter() {
            words.extend(identifiers(line).map(word_hash));
        }
    }
    for list in [&mut names, &mut top_level, &mut extension_receivers] {
        list.sort_unstable();
        list.dedup();
    }
    words.sort_unstable();
    words.dedup();
    LibraryIndex {
        jar: jar.to_string_lossy().into_owned(),
        stamp: Some(stamp),
        names,
        top_level,
        extension_receivers,
        words,
    }
}

/// Extract and parse `jar`, cache the parse and its index, and return both.
/// A parse a worker thread panicked in is not cached (a gap behind an
/// unchanged jar would never heal), but its names are still used.
fn parse_jar(
    directory: &Path,
    jar: &Path,
    stamp: Stamp,
) -> Option<(LibraryIndex, Vec<SourcesFileEntry>, bool)> {
    let entries = match super::jar::extract_sources_jar_entries(jar) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("library sources: cannot read {}: {error}", jar.display());
            return None;
        }
    };
    let parsed = super::jar::parse_jar_entries(entries);
    let files: Vec<SourcesFileEntry> = parsed
        .results
        .into_iter()
        .map(|result| {
            let mut data = result.data;
            data.source_set = SourceSet::Library;
            SourcesFileEntry {
                uri: result.uri.to_string(),
                content_hash: result.content_hash,
                file_data: Arc::new(data),
            }
        })
        .collect();
    let index = index_of(jar, stamp, &files);
    let cached = parsed.complete
        && write_file(
            &parse_path(directory, jar),
            &LibraryParse {
                stamp,
                files: files.clone(),
            },
        )
        && write_file(&index_path(directory, jar), &index);
    if !parsed.complete {
        log::warn!(
            "library sources: parse incomplete for {} — not caching it",
            jar.display()
        );
    }
    Some((index, files, cached))
}

// ── Mounting (startup) ────────────────────────────────────────────────────────

/// Make every jar in `jars` known by name without loading it: read its
/// cached index, or parse it once to build the cache. Returns the number of
/// names registered. `cache_base` overrides `~/.cache/kmp-lsp` (tests).
pub(crate) fn mount(
    indexer: &super::Indexer,
    jars: &[PathBuf],
    cache_base: Option<&Path>,
) -> usize {
    let started = Instant::now();
    let directory = cache_directory(cache_base);
    *indexer
        .library_sources
        .cache_directory
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = directory.clone();
    indexer.library_sources.jars.clear();

    let mut names = 0usize;
    let mut parsed = 0usize;
    for jar in jars {
        let Some(fingerprint) = super::sources_jar_cache::jar_fingerprint(jar) else {
            log::warn!("library sources: cannot stat {}", jar.display());
            continue;
        };
        let stamp = Stamp::from(&fingerprint);
        let cached_index = read_file::<LibraryIndex>(&index_path(&directory, jar))
            .filter(|index| index.stamp == Some(stamp))
            .filter(|_| parse_path(&directory, jar).is_file());
        let (index, cached) = match cached_index {
            Some(index) => (index, true),
            None => {
                parsed += 1;
                // The parse is dropped right away: only the index is kept.
                match parse_jar(&directory, jar, stamp) {
                    Some((index, _files, cached)) => (index, cached),
                    None => continue,
                }
            }
        };
        let jar_id = indexer.jar_table.intern(&jar.to_string_lossy());
        names += register(indexer, jar_id, index, cached);
    }
    prune_deleted_jars(&directory, jars);
    if names > 0 {
        indexer.note_jar_symbols_populated();
    }
    log::info!(
        "library sources: {} libraries known by name ({names} names, {parsed} parsed now) in {} ms — loaded on first use",
        indexer.library_sources.jars.len(),
        started.elapsed().as_millis()
    );
    names
}

/// Put `index`'s names in the Tier-1 maps. Returns how many.
fn register(indexer: &super::Indexer, jar_id: JarId, index: LibraryIndex, cached: bool) -> usize {
    for name in &index.names {
        let mut slot = indexer.jar_bare_names.entry(name.clone()).or_default();
        if !slot.contains(&jar_id) {
            slot.push(jar_id);
        }
    }
    for fqn in index.top_level {
        indexer.jar_qualified.entry(fqn).or_insert(jar_id);
    }
    for receiver in index.extension_receivers {
        let mut slot = indexer.jar_extension_receivers.entry(receiver).or_default();
        if !slot.contains(&jar_id) {
            slot.push(jar_id);
        }
    }
    indexer.library_sources.jars.insert(
        jar_id,
        LibraryJar {
            words: index.words.into(),
            parse_cached: cached,
        },
    );
    index.names.len()
}

/// Delete the cache files of jars that no longer exist (an old library
/// version Gradle cleaned up). Files of jars in `current` are kept without
/// reading them.
fn prune_deleted_jars(directory: &Path, current: &[PathBuf]) {
    let current_stems: std::collections::HashSet<String> =
        current.iter().map(|jar| cache_stem(jar)).collect();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "index")
        {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        if current_stems.contains(&stem) {
            continue;
        }
        let jar_exists =
            read_file::<LibraryIndex>(&path).is_some_and(|index| Path::new(&index.jar).exists());
        if !jar_exists {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("parse"));
        }
    }
}

// ── Loading (first use) ───────────────────────────────────────────────────────

/// Load `jar_id`'s files into the index: its cached parse, or a fresh parse
/// when the cache is missing or the jar changed. Returns whether it's loaded.
pub(crate) fn load(indexer: &super::Indexer, jar_id: JarId) -> bool {
    let _serialized = indexer
        .library_sources
        .load_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if indexer.materialized.contains(&jar_id) {
        return true;
    }
    if indexer.materialization_failed.contains(&jar_id) {
        return false;
    }
    let Some(path) = indexer.jar_table.path(jar_id) else {
        indexer.materialization_failed.insert(jar_id);
        return false;
    };
    let jar = PathBuf::from(path);
    let started = Instant::now();
    let directory = indexer.library_sources.directory();
    let Some(stamp) = super::sources_jar_cache::jar_fingerprint(&jar).map(|f| Stamp::from(&f))
    else {
        log::warn!("library sources: {} is gone", jar.display());
        indexer.materialization_failed.insert(jar_id);
        return false;
    };
    let cached = read_file::<LibraryParse>(&parse_path(&directory, &jar))
        .filter(|parse| parse.stamp == stamp)
        .map(|parse| parse.files);
    let from_cache = cached.is_some();
    let files = match cached {
        Some(files) => files,
        None => match parse_jar(&directory, &jar, stamp) {
            Some((_, files, now_cached)) => {
                if let Some(mut library) = indexer.library_sources.jars.get_mut(&jar_id) {
                    library.parse_cached = now_cached;
                }
                files
            }
            None => {
                indexer.materialization_failed.insert(jar_id);
                return false;
            }
        },
    };
    let file_count = files.len();
    let contributions = files
        .into_iter()
        .filter_map(|file| {
            let uri = Url::parse(&file.uri).ok()?;
            let supertypes = super::apply::derive_supertypes(&uri, &file.file_data);
            Some(super::apply::contributions_from_data(
                &uri,
                file.file_data,
                file.content_hash,
                &supertypes,
            ))
        })
        .collect();
    super::jar::apply_sources_contributions(indexer, contributions);
    indexer.materialized.insert(jar_id);
    indexer.invalidate_completion_cache();
    log::info!(
        "library sources: loaded {} ({file_count} files, {}) in {} ms",
        jar.display(),
        if from_cache { "from cache" } else { "parsed" },
        started.elapsed().as_millis()
    );
    true
}

/// The JDK's sources are never searched for usages (see
/// [`super::jdk_sources::is_jdk_sources_uri`]).
fn is_jdk_archive(indexer: &super::Indexer, jar_id: JarId) -> bool {
    indexer
        .jar_table
        .path(jar_id)
        .is_some_and(|path| super::jdk_sources::is_jdk_sources_archive(Path::new(&*path)))
}

/// Load the libraries that could contain a usage of a declaration spelled
/// one of `spellings`: those mentioning a spelling and — when `context` is
/// non-empty — also one of its words (the declaring package's last segment
/// or class name, without which no file can reach it). For find-references.
/// Returns how many were loaded.
pub(crate) fn load_libraries_mentioning(
    indexer: &super::Indexer,
    spellings: &[&str],
    context: &[&str],
) -> usize {
    if spellings.is_empty() {
        return 0;
    }
    let spelling_hashes: Vec<u64> = spellings.iter().map(|word| word_hash(word)).collect();
    let context_hashes: Vec<u64> = context.iter().map(|word| word_hash(word)).collect();
    let mentions =
        |words: &[u64], hashes: &[u64]| hashes.iter().any(|hash| words.binary_search(hash).is_ok());
    // Collected first: loading writes maps this iteration must not guard.
    let candidates: Vec<JarId> = indexer
        .library_sources
        .jars
        .iter()
        .filter(|entry| !indexer.materialized.contains(entry.key()))
        .filter(|entry| !is_jdk_archive(indexer, *entry.key()))
        .filter(|entry| {
            mentions(&entry.words, &spelling_hashes)
                && (context_hashes.is_empty() || mentions(&entry.words, &context_hashes))
        })
        .map(|entry| *entry.key())
        .collect();
    if candidates.len() > MAX_JARS_PER_REFERENCES {
        log::info!(
            "references: {} libraries mention {spellings:?}; loading the first {MAX_JARS_PER_REFERENCES}",
            candidates.len()
        );
    }
    candidates
        .into_iter()
        .take(MAX_JARS_PER_REFERENCES)
        .filter(|jar_id| load(indexer, *jar_id))
        .count()
}

#[cfg(test)]
#[path = "library_sources_tests.rs"]
mod tests;

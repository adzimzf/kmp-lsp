//! The JDK's own sources (`java.*`, `javax.*`), indexed like any library
//! sources jar so `ArrayList`, `StringBuilder`, `java.time.Instant`, … have
//! real declarations: typed receivers, go-to-definition into the JDK, and
//! find-references that can tell `list.add(..)` from an unrelated `add`.
//!
//! Nothing is hard-coded: the JDK is looked up for each project, and the
//! first one that ships its sources (`lib/src.zip`) wins —
//!
//! 1. the configured one: `indexingOptions.jdkHome`, then `workspace.json`'s
//!    `jdkHome`;
//! 2. the environment's: `$JAVA_HOME`, the login shell's `JAVA_HOME`, `java`
//!    on the `PATH` (the editor's, then the login shell's), and on macOS
//!    `/usr/libexec/java_home`.
//!
//! When none does (a JRE, or a Linux JDK without its separate
//! `openjdk-NN-source` package), the JDK simply isn't indexed.
//! `KMP_LSP_JDK_SOURCES=off` turns the lookup off.

use std::path::{Path, PathBuf};

use crate::dependency_sync::decompile::JAVA_INSTALL;
use crate::dependency_sync::{
    installed_tool, login_shell_environment, login_shell_path, search_dirs,
};

/// The file name a JDK gives its sources archive (`lib/src.zip` since JDK 9,
/// `src.zip` at the root before). Library sources jars are always
/// `*-sources.jar`, so the name alone tells a JDK archive apart.
const JDK_SOURCES_ARCHIVE: &str = "src.zip";

/// The modules whose public API is indexed. Everyday JVM code lives here;
/// AWT/Swing (`java.desktop`, as large as `java.base`) and the rest stay out
/// to keep the one-time parse short.
const INDEXED_MODULES: &[&str] = &[
    "java.base",
    "java.logging",
    "java.net.http",
    "java.prefs",
    "java.sql",
    "java.xml",
];

/// Set to `off` to never index a JDK's sources.
const JDK_SOURCES_SWITCH: &str = "KMP_LSP_JDK_SOURCES";

/// Only the public packages; `sun.*`, `jdk.internal.*` and `com.sun.*` are
/// implementation details no project code should reach.
const INDEXED_PACKAGE_ROOTS: &[&str] = &["java/", "javax/"];

/// Where the JDK whose sources are indexed was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JdkOrigin {
    /// `indexingOptions.jdkHome` or `workspace.json`'s `jdkHome`.
    Configured,
    /// `$JAVA_HOME` (the editor's, then the login shell's).
    JavaHome,
    /// The `java` on the `PATH` (the editor's, then the login shell's).
    JavaOnPath,
    /// macOS's default JDK (`/usr/libexec/java_home`).
    SystemDefault,
}

/// A JDK that ships its sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JdkSources {
    pub(crate) home: PathBuf,
    /// Its `src.zip`, mounted like a library sources jar.
    pub(crate) archive: PathBuf,
    pub(crate) origin: JdkOrigin,
}

/// The JDK the user configured: the editor's `jdkHome` setting (relative to
/// the workspace root) or `workspace.json`'s `jdkHome`.
pub(crate) fn configured_jdk_home(indexer: &super::Indexer) -> Option<PathBuf> {
    let root = indexer.workspace_root.get();
    let editor_setting = indexer
        .configured_jdk_home
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    match (editor_setting, &root) {
        (Some(home), Some(root)) => Some(crate::workspace_json::resolve_workspace_path(
            &home.to_string_lossy(),
            root,
        )),
        (Some(home), None) => Some(home),
        (None, Some(root)) => crate::workspace_json::load_configured_jdk_home(root),
        (None, None) => None,
    }
}

/// The JDK sources to index for `indexer`'s project, if any JDK has them.
pub(crate) fn jdk_sources_for(indexer: &super::Indexer) -> Option<JdkSources> {
    let switched_off = std::env::var(JDK_SOURCES_SWITCH).is_ok_and(|value| value == "off");
    if switched_off {
        log::info!("jdk: not indexing JDK sources ({JDK_SOURCES_SWITCH}=off)");
        return None;
    }
    let configured = configured_jdk_home(indexer);
    let found = find_jdk_sources(configured.as_deref(), environment_jdk_homes());
    match &found {
        Some(jdk) => log::info!(
            "jdk: indexing sources of {} (found via {:?})",
            jdk.home.display(),
            jdk.origin
        ),
        None => log::info!(
            "jdk: no JDK with sources found — set indexingOptions.jdkHome or \
             workspace.json \"jdkHome\", or install the JDK's sources \
             (e.g. the openjdk-NN-source package)"
        ),
    }
    found
}

/// The first JDK, the configured one ahead of `environment`'s, that ships
/// its sources. `environment` is lazy: the login shell is only asked when
/// nothing before it has sources.
pub(crate) fn find_jdk_sources(
    configured: Option<&Path>,
    environment: impl IntoIterator<Item = (JdkOrigin, PathBuf)>,
) -> Option<JdkSources> {
    let configured = configured.map(|home| (JdkOrigin::Configured, home.to_path_buf()));
    configured
        .into_iter()
        .chain(environment)
        .find_map(|(origin, home)| match sources_archive(&home) {
            Some(archive) => Some(JdkSources {
                home,
                archive,
                origin,
            }),
            None => {
                log::info!(
                    "jdk: {} ({origin:?}) has no {JDK_SOURCES_ARCHIVE}",
                    home.display()
                );
                None
            }
        })
}

/// One place the environment may name a JDK home, asked only when reached.
type JdkHomeLookup = (JdkOrigin, fn() -> Option<PathBuf>);

/// The environment's JDK homes, in lookup order, each found only when asked.
pub(crate) fn environment_jdk_homes() -> impl Iterator<Item = (JdkOrigin, PathBuf)> {
    let lookups: [JdkHomeLookup; 5] = [
        (JdkOrigin::JavaHome, || {
            std::env::var_os("JAVA_HOME").map(PathBuf::from)
        }),
        (JdkOrigin::JavaHome, || {
            login_shell_environment()
                .java_home
                .as_ref()
                .map(PathBuf::from)
        }),
        (JdkOrigin::JavaOnPath, || {
            installed_tool("java", &search_dirs(&JAVA_INSTALL))
                .and_then(|java| home_of_java_executable(&java))
        }),
        (JdkOrigin::JavaOnPath, || {
            installed_tool("java", login_shell_path())
                .and_then(|java| home_of_java_executable(&java))
        }),
        (JdkOrigin::SystemDefault, macos_default_jdk_home),
    ];
    lookups
        .into_iter()
        .filter_map(|(origin, lookup)| Some((origin, lookup()?)))
}

/// The JDK's compiled modules to index when no JDK found for the project
/// ships its sources: the first JDK's `jmods/<module>.jmod` for each indexed
/// module (a JMOD holds the module's class files — what jdtls reads). Empty
/// when some JDK has sources (those are indexed instead, with their docs and
/// parameter names) or no JDK ships JMODs (a JRE, or a JDK 24+ built
/// without them).
pub(crate) fn jdk_compiled_modules_for(indexer: &super::Indexer) -> Vec<PathBuf> {
    let switched_off = std::env::var(JDK_SOURCES_SWITCH).is_ok_and(|value| value == "off");
    if switched_off {
        return Vec::new();
    }
    let configured = configured_jdk_home(indexer);
    let found = find_jdk_compiled_modules(configured.as_deref(), environment_jdk_homes());
    if let Some((home, modules)) = &found {
        log::info!(
            "jdk: no JDK sources — indexing the {} compiled modules of {}",
            modules.len(),
            home.display()
        );
    }
    found.map(|(_, modules)| modules).unwrap_or_default()
}

/// [`jdk_compiled_modules_for`]'s lookup: `None` when any of the JDKs has
/// sources, else the first JDK home with JMODs and those JMODs.
pub(crate) fn find_jdk_compiled_modules(
    configured: Option<&Path>,
    environment: impl IntoIterator<Item = (JdkOrigin, PathBuf)>,
) -> Option<(PathBuf, Vec<PathBuf>)> {
    let homes: Vec<PathBuf> = configured
        .map(Path::to_path_buf)
        .into_iter()
        .chain(environment.into_iter().map(|(_, home)| home))
        .collect();
    if homes.iter().any(|home| sources_archive(home).is_some()) {
        return None;
    }
    homes.into_iter().find_map(|home| {
        let modules = compiled_modules(&home);
        (!modules.is_empty()).then_some((home, modules))
    })
}

/// `home`'s JMODs for the indexed modules.
fn compiled_modules(home: &Path) -> Vec<PathBuf> {
    INDEXED_MODULES
        .iter()
        .map(|module| home.join("jmods").join(format!("{module}.jmod")))
        .filter(|jmod| jmod.is_file())
        .collect()
}

/// `home`'s sources archive, when it ships one (a dangling `src.zip`
/// symlink — Debian/Ubuntu without `openjdk-NN-source` — counts as none).
pub(crate) fn sources_archive(home: &Path) -> Option<PathBuf> {
    [
        home.join("lib").join(JDK_SOURCES_ARCHIVE),
        home.join(JDK_SOURCES_ARCHIVE),
    ]
    .into_iter()
    .find(|archive| archive.is_file())
}

/// The JDK home a `java` launcher belongs to: `<home>/bin/java`, through any
/// symlinks (`/usr/bin/java` → `/etc/alternatives/java` → the JDK). A JDK 8
/// launcher under `<home>/jre/bin` belongs to `<home>`.
pub(crate) fn home_of_java_executable(java: &Path) -> Option<PathBuf> {
    let launcher = std::fs::canonicalize(java).ok()?;
    let home = launcher.parent()?.parent()?;
    let is_jdk8_jre = home.file_name().is_some_and(|name| name == "jre");
    if is_jdk8_jre {
        return home.parent().map(Path::to_path_buf);
    }
    Some(home.to_path_buf())
}

fn macos_default_jdk_home() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = std::process::Command::new("/usr/libexec/java_home")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let home = String::from_utf8(output.stdout).ok()?;
    let home = home.trim();
    (!home.is_empty()).then(|| PathBuf::from(home))
}

/// Whether `archive` is a JDK's sources archive rather than a library's.
pub(crate) fn is_jdk_sources_archive(archive: &Path) -> bool {
    archive
        .file_name()
        .is_some_and(|name| name == JDK_SOURCES_ARCHIVE)
}

/// Whether `uri` is an entry of a JDK sources archive
/// (`jar:file:///…/lib/src.zip!/java.base/java/util/List.java`).
///
/// The JDK is indexed for its declarations, not as a place to look for
/// usages: find-references on `list.add` searching the JDK's own sources
/// would check thousands of `add` calls for every query and return the
/// JDK's internals, never project code.
pub(crate) fn is_jdk_sources_uri(uri: &str) -> bool {
    uri.split_once("!/")
        .is_some_and(|(archive, _)| archive.ends_with(&format!("/{JDK_SOURCES_ARCHIVE}")))
}

/// Whether a JDK sources archive's `entry` is indexed: a public package of
/// an indexed module. JDK 9+ archives prefix each entry with its module
/// (`java.base/java/util/List.java`); JDK 8 ones don't.
pub(crate) fn is_indexed_jdk_entry(entry: &str) -> bool {
    let package_path = match entry.split_once('/') {
        Some((module, rest)) if module.contains('.') => {
            if !INDEXED_MODULES.contains(&module) {
                return false;
            }
            rest
        }
        _ => entry,
    };
    INDEXED_PACKAGE_ROOTS
        .iter()
        .any(|root| package_path.starts_with(root))
}

//! Dependency sync: ask the project's own build tool which libraries it
//! depends on, and have it download them — sources included.
//!
//! kmp-lsp otherwise only sees what happens to be in `~/.gradle/caches`:
//! every cached version of everything, and no `-sources.jar` unless an IDE
//! once asked for it. Like jdtls (Buildship/m2e) and IntelliJ, this runs
//! the real build tool instead:
//!
//! - **Gradle** — an init script ([`gradle`]) resolves every project's
//!   compile/runtime classpath and requests each module's sources jar, which
//!   downloads it into the Gradle cache.
//! - **Maven** — `maven-dependency-plugin`'s `sources` + `list` goals
//!   ([`maven`]) download into the local repository and list the paths.
//!
//! The result is a [`DependencyManifest`] cached per workspace, keyed by a
//! fingerprint of the build files, so an unchanged project never re-runs the
//! build tool. The JAR pipeline indexes the manifest's jars and sources jars
//! (`index_sources_jars`, `index_compiled_jars_phase`); the backend's
//! watcher (`backend::dependency_watcher`) re-syncs when build files change.

pub(crate) mod gradle;
pub(crate) mod maven;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bumped whenever the way dependencies are resolved changes, so manifests
/// written by an older kmp-lsp are re-synced instead of trusted.
const MANIFEST_FORMAT: u32 = 1;

/// A build-tool run that takes longer than this is killed; the sync counts
/// as failed and is retried only after the build files change.
const SYNC_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long a manifest with failed sources downloads is trusted before the
/// sync runs again.
const INCOMPLETE_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Directory names never searched for build files: build output, VCS and
/// tool caches.
const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".gradle",
    ".idea",
    ".kotlin",
    "build",
    "target",
    "out",
    "node_modules",
];

/// Build files deeper than this below the root are not fingerprinted.
const MAX_BUILD_FILE_DEPTH: usize = 6;

/// Whether the server syncs dependencies on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SyncMode {
    /// Sync in the background on startup and whenever build files change.
    #[default]
    Auto,
    /// Never run the build tool; only what is already cached is used.
    Off,
}

impl SyncMode {
    /// `"auto"` / `"off"` (case-insensitive); anything else is `None`.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(SyncMode::Auto),
            "off" => Some(SyncMode::Off),
            _ => None,
        }
    }
}

/// The build tool that owns the workspace, and the command that runs it —
/// the project's wrapper when it has one, so the project's own tool version
/// is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildTool {
    Gradle { launcher: PathBuf },
    Maven { launcher: PathBuf },
}

impl BuildTool {
    /// Detect from the files at `root`: Gradle settings/build scripts, else a
    /// `pom.xml`. `None` for anything else (Bazel, plain sources, …).
    pub(crate) fn detect(root: &Path) -> Option<Self> {
        let has_any = |names: &[&str]| names.iter().any(|name| root.join(name).is_file());
        let is_gradle = has_any(&[
            "settings.gradle.kts",
            "settings.gradle",
            "build.gradle.kts",
            "build.gradle",
        ]);
        if is_gradle {
            let launcher = wrapper(root, "gradlew").unwrap_or_else(|| PathBuf::from("gradle"));
            return Some(BuildTool::Gradle { launcher });
        }
        if has_any(&["pom.xml"]) {
            let launcher = wrapper(root, "mvnw").unwrap_or_else(|| PathBuf::from("mvn"));
            return Some(BuildTool::Maven { launcher });
        }
        None
    }

    fn name(&self) -> &'static str {
        match self {
            BuildTool::Gradle { .. } => "gradle",
            BuildTool::Maven { .. } => "maven",
        }
    }
}

/// `root/<name>` (or `<name>.bat`/`<name>.cmd` on Windows) when present.
fn wrapper(root: &Path, name: &str) -> Option<PathBuf> {
    let file_names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.bat"), format!("{name}.cmd")]
    } else {
        vec![name.to_owned()]
    };
    file_names
        .into_iter()
        .map(|file_name| root.join(file_name))
        .find(|path| path.is_file())
}

/// A command running `launcher`. A Unix wrapper script runs through `sh`, so
/// a checkout that lost the executable bit still works.
fn launch(launcher: &Path) -> Command {
    let is_wrapper_script = !cfg!(windows) && launcher.is_file();
    if is_wrapper_script {
        let mut command = Command::new("sh");
        command.arg(launcher);
        command
    } else {
        Command::new(launcher)
    }
}

/// One resolved library: its compiled jar and, when the repository has one,
/// its sources jar. Paths point into the build tool's own cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResolvedDependency {
    /// `group:artifact:version`.
    pub(crate) coordinate: String,
    pub(crate) jar: Option<PathBuf>,
    pub(crate) sources: Option<PathBuf>,
}

/// A build tool's answer: the dependencies, and whether some sources jar
/// failed to download for a transient reason.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) dependencies: Vec<ResolvedDependency>,
    pub(crate) incomplete_sources: bool,
}

/// What the last successful sync resolved, and for which build files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DependencyManifest {
    pub(crate) format: u32,
    pub(crate) fingerprint: String,
    pub(crate) build_tool: String,
    pub(crate) dependencies: Vec<ResolvedDependency>,
    /// Some sources jar failed to download for a transient reason (network,
    /// rate limiting): re-sync after [`INCOMPLETE_RETRY_AFTER`] even though
    /// the build files are unchanged.
    #[serde(default)]
    pub(crate) incomplete_sources: bool,
    /// Seconds since the Unix epoch when the manifest was written.
    #[serde(default)]
    pub(crate) saved_at: u64,
}

impl DependencyManifest {
    /// Compiled jars that still exist on disk.
    pub(crate) fn jars(&self) -> Vec<PathBuf> {
        existing(
            self.dependencies
                .iter()
                .filter_map(|dependency| dependency.jar.clone()),
        )
    }

    /// `(group, artifact, sources jar)` for every dependency whose sources
    /// jar still exists on disk.
    pub(crate) fn sources_jars_by_artifact(&self) -> Vec<(String, String, PathBuf)> {
        self.dependencies
            .iter()
            .filter_map(|dependency| {
                let sources = dependency.sources.clone().filter(|path| path.is_file())?;
                let mut parts = dependency.coordinate.split(':');
                let group = parts.next()?.to_owned();
                let artifact = parts.next()?.to_owned();
                Some((group, artifact, sources))
            })
            .collect()
    }

    /// Sources jars that still exist on disk.
    pub(crate) fn sources_jars(&self) -> Vec<PathBuf> {
        existing(
            self.dependencies
                .iter()
                .filter_map(|dependency| dependency.sources.clone()),
        )
    }
}

fn existing(paths: impl Iterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for path in paths {
        if path.is_file() && !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

/// What one [`sync`] call did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SyncOutcome {
    /// The workspace has no Gradle/Maven build.
    NoBuildTool,
    /// The saved manifest already matches the build files.
    UpToDate,
    /// The build tool ran; a new manifest is saved.
    Synced { dependencies: usize, sources: usize },
    /// The build tool failed or couldn't be started; the message says why.
    Failed(String),
}

/// The fingerprint of `root`'s build files, `None` without a Gradle/Maven
/// build. Changes whenever any build script, settings file, version catalog,
/// `gradle.properties`, wrapper version or `pom.xml` changes.
pub(crate) fn build_fingerprint(root: &Path) -> Option<String> {
    let tool = BuildTool::detect(root)?;
    let mut build_files: Vec<PathBuf> = Vec::new();
    collect_build_files(root, 0, &mut build_files);
    build_files.sort();
    let mut hasher = Sha256::new();
    hasher.update(format!("kmp-lsp dependency manifest v{MANIFEST_FORMAT}\n"));
    hasher.update(tool.name());
    for path in &build_files {
        let relative = path.strip_prefix(root).unwrap_or(path);
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(path).unwrap_or_default());
        hasher.update([0]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// Whether a file name is one whose change can change the dependencies.
pub(crate) fn is_build_file_name(file_name: &str) -> bool {
    matches!(
        file_name,
        "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "settings.gradle.kts"
            | "gradle.properties"
            | "gradle-wrapper.properties"
            | "pom.xml"
    ) || file_name.ends_with(".versions.toml")
}

fn collect_build_files(directory: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            let descend =
                depth < MAX_BUILD_FILE_DEPTH && !SKIPPED_DIRECTORIES.contains(&name.as_ref());
            if descend {
                collect_build_files(&entry.path(), depth + 1, found);
            }
        } else if file_type.is_file() && is_build_file_name(&name) {
            found.push(entry.path());
        }
    }
}

/// Where the manifest for `root` is cached:
/// `~/.cache/kmp-lsp/dependencies/<hash of root>.json`.
fn manifest_path(root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    dependencies_cache_dir().join(format!("{}.json", &key[..16]))
}

fn dependencies_cache_dir() -> PathBuf {
    crate::indexer::xdg_cache_base()
        .join("kmp-lsp")
        .join("dependencies")
}

/// The last saved manifest for `root`, current or not. A stale manifest is
/// still the best list of the project's libraries until a re-sync finishes.
pub(crate) fn load_manifest(root: &Path) -> Option<DependencyManifest> {
    let text = std::fs::read_to_string(manifest_path(root)).ok()?;
    let manifest: DependencyManifest = serde_json::from_str(&text).ok()?;
    (manifest.format == MANIFEST_FORMAT).then_some(manifest)
}

fn save_manifest(root: &Path, manifest: &DependencyManifest) -> std::io::Result<()> {
    let path = manifest_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(manifest).map_err(std::io::Error::other)?;
    // Write-then-rename so a reader never sees a half-written manifest.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path)
}

/// Whether the saved manifest was made from exactly these build files and
/// isn't due for a retry of failed sources downloads.
pub(crate) fn manifest_matches(root: &Path, fingerprint: &str) -> bool {
    load_manifest(root).is_some_and(|manifest| {
        manifest.fingerprint == fingerprint && !manifest.is_due_for_retry(unix_now())
    })
}

/// Whether the saved manifest for these build files lacks some sources
/// because of a transient download failure, and the back-off has passed.
pub(crate) fn sources_retry_due(root: &Path, fingerprint: &str) -> bool {
    load_manifest(root).is_some_and(|manifest| {
        manifest.fingerprint == fingerprint && manifest.is_due_for_retry(unix_now())
    })
}

/// The shortest wait between two automatic retries of failed sources
/// downloads, also applied when a retry itself fails.
pub(crate) const RETRY_BACK_OFF: Duration = INCOMPLETE_RETRY_AFTER;

impl DependencyManifest {
    fn is_due_for_retry(&self, now: u64) -> bool {
        self.incomplete_sources && now >= self.saved_at + INCOMPLETE_RETRY_AFTER.as_secs()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Run the build tool when the saved manifest doesn't match the build files
/// (or `force` is set), and save the new manifest. Blocking — call from a
/// blocking task.
pub(crate) fn sync(root: &Path, force: bool) -> SyncOutcome {
    let Some(tool) = BuildTool::detect(root) else {
        return SyncOutcome::NoBuildTool;
    };
    let Some(fingerprint) = build_fingerprint(root) else {
        return SyncOutcome::NoBuildTool;
    };
    if !force && manifest_matches(root, &fingerprint) {
        return SyncOutcome::UpToDate;
    }
    let work_dir = dependencies_cache_dir();
    if let Err(error) = std::fs::create_dir_all(&work_dir) {
        return SyncOutcome::Failed(format!("cannot create {}: {error}", work_dir.display()));
    }
    let output_file = manifest_path(root).with_extension("output");
    let _ = std::fs::remove_file(&output_file);
    let resolved = match &tool {
        BuildTool::Gradle { launcher } => gradle::resolve(root, launcher, &work_dir, &output_file),
        BuildTool::Maven { launcher } => maven::resolve(root, launcher, &output_file),
    };
    let _ = std::fs::remove_file(&output_file);
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(message) => return SyncOutcome::Failed(message),
    };
    let manifest = DependencyManifest {
        format: MANIFEST_FORMAT,
        fingerprint,
        build_tool: tool.name().to_owned(),
        dependencies: resolved.dependencies,
        incomplete_sources: resolved.incomplete_sources,
        saved_at: unix_now(),
    };
    if let Err(error) = save_manifest(root, &manifest) {
        return SyncOutcome::Failed(format!("cannot save the dependency manifest: {error}"));
    }
    SyncOutcome::Synced {
        dependencies: manifest.dependencies.len(),
        sources: manifest.sources_jars().len(),
    }
}

/// Run `command` to completion within [`SYNC_TIMEOUT`]. On failure, the
/// error carries the last lines the tool wrote, which is where Gradle and
/// Maven say what went wrong.
fn run_build_tool(mut command: Command, description: &str) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start {description}: {error}"))?;
    let mut stderr = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + SYNC_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{description} timed out after {SYNC_TIMEOUT:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => return Err(format!("{description} failed: {error}")),
        }
    };
    let stderr_text = stderr_reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let tail: Vec<&str> = stderr_text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("Picked up JAVA_TOOL_OPTIONS"))
        .collect();
    let tail = tail[tail.len().saturating_sub(12)..].join("\n");
    Err(format!("{description} exited with {status}:\n{tail}"))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

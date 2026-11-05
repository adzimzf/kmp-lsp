//! Decompile libraries that publish no sources jar — what IntelliJ does with
//! its bundled Fernflower — with Vineflower (the maintained Fernflower fork,
//! Apache-2.0), which turns Kotlin classes back into Kotlin and Java classes
//! into Java.
//!
//! The decompiler jar is fetched through the project's own build tool, so a
//! company mirror, proxy or credentials apply exactly as for any dependency:
//! the Gradle init script resolves it alongside the dependencies (see
//! `kmp-lsp-dependencies.gradle`), Maven copies it with `dependency:copy`.
//! It runs on the user's own `java`.
//!
//! Each library is decompiled once per jar version into a cached
//! `…-decompiled-sources.jar`, which the JAR pipeline mounts like any
//! sources jar: go-to-definition, hover and find-references then work on
//! real code. A library that fails to decompile keeps the signature stub.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    installed_tool, launch, login_shell_environment, login_shell_path, run_tool, search_dirs,
    InstallLocations, ResolvedDependency,
};

/// The decompiler, as a Maven coordinate.
pub(crate) const DECOMPILER_COORDINATE: &str = "org.vineflower:vineflower:1.11.1";
const DECOMPILER_FILE: &str = "vineflower-1.11.1.jar";

/// One library is given at most this long to decompile.
const DECOMPILE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// At most this many libraries are decompiled per sync…
const MAX_DECOMPILED_JARS: usize = 100;
/// …each no bigger than this.
const MAX_JAR_BYTES: u64 = 64 * 1024 * 1024;
/// Libraries decompiled at the same time.
const PARALLEL_DECOMPILES: usize = 4;

pub(crate) const JAVA_INSTALL: InstallLocations = InstallLocations {
    home_variables: &["JAVA_HOME"],
    sdkman_candidate: "java",
    directories: &["/usr/lib/jvm/default-java/bin"],
};

fn decompiler_dir() -> PathBuf {
    crate::indexer::xdg_cache_base()
        .join("kmp-lsp")
        .join("decompiler")
}

fn decompiled_dir() -> PathBuf {
    crate::indexer::xdg_cache_base()
        .join("kmp-lsp")
        .join("decompiled")
}

/// The decompiler jar, when an earlier sync already fetched it.
pub(crate) fn cached_decompiler() -> Option<PathBuf> {
    let jar = decompiler_dir().join(DECOMPILER_FILE);
    jar.is_file().then_some(jar)
}

/// Keep the decompiler a build tool resolved (Gradle hands back a path in its
/// own cache) where the next sync finds it without asking again.
pub(crate) fn remember_decompiler(resolved: &Path) -> Option<PathBuf> {
    let destination = decompiler_dir().join(DECOMPILER_FILE);
    if destination.is_file() {
        return Some(destination);
    }
    std::fs::create_dir_all(decompiler_dir()).ok()?;
    let temporary = destination.with_extension("jar.tmp");
    std::fs::copy(resolved, &temporary).ok()?;
    std::fs::rename(&temporary, &destination).ok()?;
    Some(destination)
}

/// Fetch the decompiler with Maven (`dependency:copy`), through the
/// project's own settings — mirrors and credentials included.
pub(crate) fn fetch_decompiler_with_maven(root: &Path, launcher: &Path) -> Result<PathBuf, String> {
    let directory = decompiler_dir();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let mut command = launch(launcher);
    command
        .current_dir(root)
        .args(["--batch-mode", "--quiet"])
        .arg(format!("{}:copy", super::maven::DEPENDENCY_PLUGIN))
        .arg(format!("-Dartifact={DECOMPILER_COORDINATE}"))
        .arg(format!("-DoutputDirectory={}", directory.display()));
    run_tool(
        command,
        "fetching the decompiler with Maven",
        super::SYNC_TIMEOUT,
    )
    .map_err(|failure| failure.message())?;
    cached_decompiler().ok_or_else(|| "Maven didn't copy the decompiler".to_owned())
}

/// The `java` to run the decompiler with: `$JAVA_HOME`, the login shell's
/// `JAVA_HOME`, then `java` on the server's or the login shell's `PATH`.
pub(crate) fn java_executable() -> Option<PathBuf> {
    let java_file = if cfg!(windows) { "java.exe" } else { "java" };
    let shell_java_home = login_shell_environment()
        .java_home
        .as_ref()
        .map(|home| PathBuf::from(home).join("bin").join(java_file))
        .filter(|java| java.is_file());
    installed_tool("java", &search_dirs(&JAVA_INSTALL))
        .or(shell_java_home)
        .or_else(|| installed_tool("java", login_shell_path()))
}

/// Decompile every dependency that has a compiled jar but no sources jar,
/// recording each result in `decompiled`. Failures are logged and leave the
/// dependency as it was.
pub(crate) fn decompile_missing_sources(
    dependencies: &mut [ResolvedDependency],
    java: &Path,
    decompiler: &Path,
) {
    let candidates: Vec<(usize, PathBuf)> = dependencies
        .iter()
        .enumerate()
        .filter(|(_, dependency)| dependency.sources.is_none())
        .filter_map(|(index, dependency)| Some((index, dependency.jar.clone()?)))
        .filter(|(_, jar)| is_decompilable(jar))
        .take(MAX_DECOMPILED_JARS)
        .collect();
    if candidates.is_empty() {
        return;
    }
    log::info!(
        "dependency sync: decompiling {} libraries without sources",
        candidates.len()
    );
    let chunk_size = candidates.len().div_ceil(PARALLEL_DECOMPILES).max(1);
    let results: Vec<(usize, PathBuf)> = std::thread::scope(|scope| {
        let workers: Vec<_> = candidates
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|(index, jar)| match decompile_jar(java, decompiler, jar) {
                            Ok(sources) => Some((*index, sources)),
                            Err(message) => {
                                log::warn!(
                                    "dependency sync: cannot decompile {}: {message}",
                                    jar.display()
                                );
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap_or_default())
            .collect()
    });
    for (index, sources) in results {
        dependencies[index].decompiled = Some(sources);
    }
}

/// A plain `.jar` (an `.aar` wraps its classes in another archive) small
/// enough to decompile.
fn is_decompilable(jar: &Path) -> bool {
    let is_jar = jar.extension().is_some_and(|extension| extension == "jar");
    let size_ok = std::fs::metadata(jar).is_ok_and(|metadata| metadata.len() <= MAX_JAR_BYTES);
    is_jar && size_ok
}

/// Where the decompiled sources of `jar` are cached: one file per jar version.
fn decompiled_jar_path(jar: &Path) -> Option<PathBuf> {
    let stem = jar.file_stem()?.to_string_lossy().into_owned();
    let fingerprint = crate::jar_extract::jar_fingerprint(jar)?;
    Some(decompiled_dir().join(format!("{stem}-{fingerprint:016x}-decompiled-sources.jar")))
}

/// Decompile `jar` (once per jar version) into a sources jar of `.kt` /
/// `.java` files and return its path.
fn decompile_jar(java: &Path, decompiler: &Path, jar: &Path) -> Result<PathBuf, String> {
    let destination =
        decompiled_jar_path(jar).ok_or_else(|| "cannot read the jar's metadata".to_owned())?;
    if destination.is_file() {
        return Ok(destination);
    }
    let work_dir = destination.with_extension("work");
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir)
        .map_err(|error| format!("cannot create {}: {error}", work_dir.display()))?;
    let mut command = std::process::Command::new(java);
    command
        .arg("-jar")
        .arg(decompiler)
        .arg("-log=WARN")
        .arg(jar)
        .arg(&work_dir);
    let outcome = run_tool(command, "the decompiler", DECOMPILE_TIMEOUT)
        .map_err(|failure| failure.message())
        .and_then(|()| package_sources(&work_dir, &destination));
    let _ = std::fs::remove_dir_all(&work_dir);
    outcome.map(|()| destination)
}

/// Zip every `.kt` / `.java` file under `directory` (at its package path)
/// into the sources jar `destination`.
pub(crate) fn package_sources(directory: &Path, destination: &Path) -> Result<(), String> {
    use std::io::Write;
    let mut files: Vec<PathBuf> = Vec::new();
    collect_sources(directory, &mut files);
    if files.is_empty() {
        return Err("the decompiler produced no sources".to_owned());
    }
    files.sort();
    let temporary = destination.with_extension("jar.tmp");
    let file = std::fs::File::create(&temporary)
        .map_err(|error| format!("cannot create {}: {error}", temporary.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for path in &files {
        let entry = path
            .strip_prefix(directory)
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        zip.start_file(entry, options)
            .map_err(|error| error.to_string())?;
        zip.write_all(&bytes).map_err(|error| error.to_string())?;
    }
    zip.finish().map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, destination).map_err(|error| error.to_string())
}

fn collect_sources(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_sources(&path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "kt" || extension == "java")
        {
            files.push(path);
        }
    }
}

//! Maven: `maven-dependency-plugin`'s `sources` goal downloads every
//! dependency's sources jar into the local repository; `list` reports each
//! resolved file's absolute path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{launch, run_build_tool, BuildToolFailure, Resolved, ResolvedDependency};

/// Pinned so the output format below never shifts under us.
pub(super) const DEPENDENCY_PLUGIN: &str = "org.apache.maven.plugins:maven-dependency-plugin:3.8.1";

pub(crate) fn resolve(
    root: &Path,
    launcher: &Path,
    output_file: &Path,
) -> Result<Resolved, BuildToolFailure> {
    let mut command = launch(launcher);
    command
        .current_dir(root)
        // One module that can't resolve must not hide every other module's
        // dependencies: a reactor module depending on a sibling's SNAPSHOT
        // that was never installed fails these goals (no `package` phase
        // runs), while the modules that matter resolve fine.
        .args(["--batch-mode", "--quiet", "--fail-at-end"])
        .arg(format!("{DEPENDENCY_PLUGIN}:sources"))
        .arg(format!("{DEPENDENCY_PLUGIN}:list"))
        .arg("-DoutputAbsoluteArtifactFilename=true")
        .arg(format!("-DoutputFile={}", output_file.display()))
        // Every module of a multi-module build appends to the same file.
        .arg("-DappendOutput=true");
    let run = run_build_tool(
        command,
        &format!(
            "`{} dependency:sources dependency:list`",
            launcher.display()
        ),
    );
    let modules = match run {
        Ok(()) => ModulesResolved::All,
        Err(BuildToolFailure::Failed(message)) => ModulesResolved::Some { failure: message },
        Err(not_found @ BuildToolFailure::NotFound(_)) => return Err(not_found),
    };
    let text = std::fs::read_to_string(output_file).unwrap_or_default();
    resolved_from(parse_output(&text), modules)
}

/// Whether every module of the build resolved its dependencies.
pub(crate) enum ModulesResolved {
    All,
    /// Maven failed, but the modules it got through before (or around, with
    /// `--fail-at-end`) the failure may still have written their list.
    Some {
        failure: String,
    },
}

/// The sync result for the dependencies Maven listed. A partial list is
/// kept — the libraries of the modules that resolved are still the right
/// ones — and marked incomplete so a later sync tries again; an empty list
/// from a failed run is the failure itself.
pub(crate) fn resolved_from(
    dependencies: Vec<ResolvedDependency>,
    modules: ModulesResolved,
) -> Result<Resolved, BuildToolFailure> {
    let incomplete = match modules {
        ModulesResolved::All => false,
        ModulesResolved::Some { failure } if dependencies.is_empty() => {
            return Err(BuildToolFailure::Failed(failure));
        }
        ModulesResolved::Some { failure } => {
            log::warn!(
                "dependency sync: some Maven modules could not resolve; using the {} \
                 dependencies of the others: {failure}",
                dependencies.len()
            );
            true
        }
    };
    // Maven's `sources` goal reports a missing and a failed download alike
    // ("NOT been resolved"), so a complete Maven result is never marked
    // incomplete.
    Ok(Resolved {
        dependencies,
        incomplete_sources: incomplete,
        decompiler: None,
    })
}

/// Parse `dependency:sources` / `dependency:list` output. Both goals write
/// `group:artifact:type[:classifier]:version[:scope]:/abs/path` lines (the
/// two goals order the fields differently), optionally followed by
/// ` -- module …`. A path ending in `-sources.jar` is a sources jar; the
/// version is the field the file name is built from (`artifact-VERSION…`).
pub(crate) fn parse_output(text: &str) -> Vec<ResolvedDependency> {
    let mut by_coordinate: BTreeMap<String, ResolvedDependency> = BTreeMap::new();
    for line in text.lines() {
        let Some((fields, path)) = split_dependency_line(line) else {
            continue;
        };
        let [group, artifact, ..] = fields.as_slice() else {
            continue;
        };
        // Split on both separators: a Windows path read on any host.
        let path_text = path.to_string_lossy().into_owned();
        let file_name = path_text.rsplit(['/', '\\']).next().unwrap_or_default();
        let Some(version) = fields[2..]
            .iter()
            .find(|field| file_name.starts_with(&format!("{artifact}-{field}")))
        else {
            continue;
        };
        let coordinate = format!("{group}:{artifact}:{version}");
        let entry = by_coordinate
            .entry(coordinate.clone())
            .or_insert_with(|| ResolvedDependency {
                coordinate,
                jar: None,
                sources: None,
                decompiled: None,
            });
        if file_name.ends_with("-sources.jar") {
            entry.sources = Some(path);
        } else if file_name.ends_with(".jar") {
            entry.jar = Some(path);
        }
    }
    by_coordinate.into_values().collect()
}

/// `(colon-separated fields, absolute path)` of one artifact line, or `None`
/// for headers and blank lines. The path starts at the first `:/` (Unix) or
/// `:X:\` / `:X:/` (Windows drive) — the fields themselves never contain one.
fn split_dependency_line(line: &str) -> Option<(Vec<&str>, PathBuf)> {
    let line = line.trim();
    let line = line.split(" -- ").next().unwrap_or(line);
    let bytes = line.as_bytes();
    let path_start = (0..bytes.len().saturating_sub(1)).find(|&index| {
        let unix_path = bytes[index] == b':' && bytes[index + 1] == b'/';
        let windows_path = bytes[index] == b':'
            && bytes.get(index + 2) == Some(&b':')
            && bytes[index + 1].is_ascii_alphabetic()
            && matches!(bytes.get(index + 3), Some(b'\\') | Some(b'/'));
        unix_path || windows_path
    })?;
    let fields: Vec<&str> = line[..path_start].split(':').collect();
    if fields.len() < 4 {
        return None;
    }
    Some((fields, PathBuf::from(&line[path_start + 1..])))
}

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
    let first = run_dependency_goals(root, launcher, output_file, SiblingModules::FromRepository)?;
    let ModulesResolved::Some { .. } = &first else {
        return finish(output_file, first);
    };
    // A module depending on a sibling's SNAPSHOT that was never installed
    // can't resolve from the local repository. Run again with the `compile`
    // phase in the session, which lets Maven's reactor serve each sibling
    // from its own build output instead.
    let first_output = std::fs::read_to_string(output_file).unwrap_or_default();
    let _ = std::fs::remove_file(output_file);
    match run_dependency_goals(root, launcher, output_file, SiblingModules::FromReactor) {
        Ok(ModulesResolved::All) => finish(output_file, ModulesResolved::All),
        _ => {
            let _ = std::fs::write(output_file, first_output);
            finish(output_file, first)
        }
    }
}

/// Where a reactor module's dependency on a sibling module is resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SiblingModules {
    /// The local repository, as a plain `dependency:list` run does: only a
    /// sibling that was installed there resolves.
    FromRepository,
    /// The reactor: the `compile` phase joins the session (with compilation
    /// itself skipped), so Maven serves each sibling from its build output.
    FromReactor,
}

pub(crate) fn dependency_goal_arguments(
    output_file: &Path,
    siblings: SiblingModules,
) -> Vec<String> {
    // One module that can't resolve must not hide every other module's
    // dependencies: `--fail-at-end` keeps going past it.
    let mut arguments: Vec<String> = ["--batch-mode", "--quiet", "--fail-at-end"]
        .map(str::to_owned)
        .to_vec();
    if siblings == SiblingModules::FromReactor {
        arguments.push("-Dmaven.main.skip=true".to_owned());
        arguments.push("compile".to_owned());
    }
    arguments.extend([
        format!("{DEPENDENCY_PLUGIN}:sources"),
        format!("{DEPENDENCY_PLUGIN}:list"),
        "-DoutputAbsoluteArtifactFilename=true".to_owned(),
        format!("-DoutputFile={}", output_file.display()),
        // Every module of a multi-module build appends to the same file.
        "-DappendOutput=true".to_owned(),
    ]);
    arguments
}

fn run_dependency_goals(
    root: &Path,
    launcher: &Path,
    output_file: &Path,
    siblings: SiblingModules,
) -> Result<ModulesResolved, BuildToolFailure> {
    let mut command = launch(launcher);
    command
        .current_dir(root)
        .args(dependency_goal_arguments(output_file, siblings));
    let run = run_build_tool(
        command,
        &format!(
            "`{} dependency:sources dependency:list`",
            launcher.display()
        ),
    );
    match run {
        Ok(()) => Ok(ModulesResolved::All),
        Err(BuildToolFailure::Failed(message)) => Ok(ModulesResolved::Some { failure: message }),
        Err(not_found @ BuildToolFailure::NotFound(_)) => Err(not_found),
    }
}

fn finish(output_file: &Path, modules: ModulesResolved) -> Result<Resolved, BuildToolFailure> {
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

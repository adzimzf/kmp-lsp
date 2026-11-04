//! Maven: `maven-dependency-plugin`'s `sources` goal downloads every
//! dependency's sources jar into the local repository; `list` reports each
//! resolved file's absolute path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{launch, run_build_tool, Resolved, ResolvedDependency};

/// Pinned so the output format below never shifts under us.
const DEPENDENCY_PLUGIN: &str = "org.apache.maven.plugins:maven-dependency-plugin:3.8.1";

pub(crate) fn resolve(
    root: &Path,
    launcher: &Path,
    output_file: &Path,
) -> Result<Resolved, String> {
    let mut command = launch(launcher);
    command
        .current_dir(root)
        .args(["--batch-mode", "--quiet"])
        .arg(format!("{DEPENDENCY_PLUGIN}:sources"))
        .arg(format!("{DEPENDENCY_PLUGIN}:list"))
        .arg("-DoutputAbsoluteArtifactFilename=true")
        .arg(format!("-DoutputFile={}", output_file.display()))
        // Every module of a multi-module build appends to the same file.
        .arg("-DappendOutput=true");
    run_build_tool(
        command,
        &format!(
            "`{} dependency:sources dependency:list`",
            launcher.display()
        ),
    )?;
    let text = std::fs::read_to_string(output_file)
        .map_err(|error| format!("Maven wrote no dependency list: {error}"))?;
    // Maven's `sources` goal reports a missing and a failed download alike
    // ("NOT been resolved"), so a Maven result is never marked incomplete.
    Ok(Resolved {
        dependencies: parse_output(&text),
        incomplete_sources: false,
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

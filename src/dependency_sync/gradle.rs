//! Gradle: resolve dependencies and download their sources through an init
//! script (`kmp-lsp-dependencies.gradle`), using the project's own Gradle.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{launch, run_build_tool, BuildToolFailure, Resolved, ResolvedDependency};

const INIT_SCRIPT: &str = include_str!("kmp-lsp-dependencies.gradle");
const INIT_SCRIPT_FILE: &str = "kmp-lsp-dependencies.gradle";
const TASK: &str = "kmpLspDependencies";

/// One line of the init script's output: a dependency, or the trailing
/// `{"incompleteSources":true}` marker.
#[derive(Deserialize)]
#[serde(untagged)]
enum GradleOutputLine {
    Dependency {
        coordinate: String,
        jar: Option<PathBuf>,
        sources: Option<PathBuf>,
    },
    Status {
        #[serde(rename = "incompleteSources")]
        incomplete_sources: bool,
    },
}

/// Run the init script's task in `root` with `launcher` (`./gradlew` or
/// `gradle`) and read back what it resolved.
pub(crate) fn resolve(
    root: &Path,
    launcher: &Path,
    work_dir: &Path,
    output_file: &Path,
) -> Result<Resolved, BuildToolFailure> {
    let script = work_dir.join(INIT_SCRIPT_FILE);
    std::fs::write(&script, INIT_SCRIPT)
        .map_err(|error| format!("cannot write {}: {error}", script.display()))?;
    let mut command = launch(launcher);
    command
        .current_dir(root)
        .arg("--init-script")
        .arg(&script)
        .args(["--quiet", "--console=plain"])
        // The task reads the whole model at execution time, which the
        // configuration cache rejects; the project's own setting must not
        // make the sync fail.
        .arg("--no-configuration-cache")
        .arg(format!("-DkmpLsp.output={}", output_file.display()))
        .arg(TASK);
    run_build_tool(command, &format!("`{} {TASK}`", launcher.display()))?;
    let text = std::fs::read_to_string(output_file)
        .map_err(|error| format!("Gradle wrote no dependency list: {error}"))?;
    Ok(parse_output(&text))
}

/// Parse the init script's JSON-lines output; unreadable lines are skipped.
pub(crate) fn parse_output(text: &str) -> Resolved {
    let mut resolved = Resolved::default();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<GradleOutputLine>(line) {
            Ok(GradleOutputLine::Dependency {
                coordinate,
                jar,
                sources,
            }) => resolved.dependencies.push(ResolvedDependency {
                coordinate,
                jar,
                sources,
            }),
            Ok(GradleOutputLine::Status { incomplete_sources }) => {
                resolved.incomplete_sources |= incomplete_sources;
            }
            Err(_) => {}
        }
    }
    resolved
}

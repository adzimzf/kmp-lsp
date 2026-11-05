//! The CLI loads the project's dependencies the way the LSP does: library
//! sources jars from the Gradle cache are mounted (parsed in Rust, so this
//! holds without the `kmp-jar-indexer` sidecar), unless `--no-deps`.

mod common;

use std::process::{Command, Output};

use common::{copy_dir, textkit, write};

const BIN: &str = env!("CARGO_BIN_EXE_kmp-lsp");

/// The `app` fixture, with `textkit-1.0.0-sources.jar` in its Gradle cache.
/// `HOME` and the cache directory are isolated so nothing real leaks in.
fn run_in_textkit_workspace(args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("app");
    std::fs::create_dir_all(&root).unwrap();
    copy_dir(&textkit::fixture_dir().join("app"), &root);
    write(&root, "workspace.json", r#"{"sourcePaths":[]}"#);
    let gradle_user_home = dir.path().join("gradle-home");
    textkit::install_sources_in_gradle_cache(&gradle_user_home);

    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .arg("--root")
            .arg(&root)
            .env("HOME", dir.path())
            .env("GRADLE_USER_HOME", &gradle_user_home)
            .env("XDG_CACHE_HOME", dir.path().join("xdg-cache"))
            .env_remove("ANDROID_HOME")
            .env_remove("ANDROID_SDK_ROOT")
            .output()
            .expect("spawn")
    };
    // `--smart` needs a pre-built index.
    assert!(run(&["index"]).status.success(), "kmp-lsp index failed");
    run(args)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn is_slugger_in_sources_jar(line: &str) -> bool {
    let in_sources_jar = line.contains("textkit-1.0.0-sources.jar!");
    let in_slugger = line.contains("com/acme/textkit/Slugger.kt");
    in_sources_jar && in_slugger
}

#[test]
fn find_resolves_a_library_class_from_its_sources_jar_by_default() {
    let output = run_in_textkit_workspace(&["find", "Slugger", "--smart"]);
    let found = stdout(&output);
    assert!(
        found.lines().any(is_slugger_in_sources_jar),
        "expected `Slugger` declared in textkit's sources jar:\n{found}"
    );
}

#[test]
fn find_with_no_deps_leaves_library_sources_unmounted() {
    let output = run_in_textkit_workspace(&["find", "Slugger", "--smart", "--no-deps"]);
    let found = stdout(&output);
    assert!(
        !found.lines().any(is_slugger_in_sources_jar),
        "expected no library declaration with --no-deps:\n{found}"
    );
}

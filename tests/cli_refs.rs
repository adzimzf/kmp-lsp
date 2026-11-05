//! Integration tests for `kmp-lsp refs`.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kmp-lsp");

fn write_fixture(dir: &Path, rel_path: &str, content: &str) {
    let full = dir.join(rel_path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full, content).unwrap();
}

#[test]
fn exclude_imports_removes_import_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    // src/A.kt imports Foo and uses it as a real reference.
    write_fixture(
        root,
        "src/A.kt",
        "import com.example.Foo\n\nfun useIt(f: Foo) {}\n",
    );
    write_fixture(root, "src/B.kt", "class Foo\n");

    let out_default = Command::new(BIN)
        .args(["refs", "Foo", "--fast", "--root"])
        .arg(root)
        .output()
        .expect("spawn");
    let default_output = String::from_utf8_lossy(&out_default.stdout);

    let out_excluded = Command::new(BIN)
        .args(["refs", "Foo", "--fast", "--exclude-imports", "--root"])
        .arg(root)
        .output()
        .expect("spawn");
    let excluded_output = String::from_utf8_lossy(&out_excluded.stdout);

    // Default output includes the import on line 1 of A.kt.
    let default_lines: Vec<&str> = default_output.lines().collect();
    let excluded_lines: Vec<&str> = excluded_output.lines().collect();

    // The import is on line 1; check default output has an A.kt line-1 entry.
    assert!(
        default_lines
            .iter()
            .any(|line| line.contains("A.kt") && line.contains(":1:")),
        "expected import (A.kt:1:...) in default refs output:\n{default_output}"
    );
    // With --exclude-imports the A.kt:1 entry must be gone.
    assert!(
        !excluded_lines
            .iter()
            .any(|line| line.contains("A.kt") && line.contains(":1:")),
        "expected no import line (A.kt:1:...) with --exclude-imports:\n{excluded_output}"
    );
    // But the real usage on line 3 must still appear.
    assert!(
        excluded_lines
            .iter()
            .any(|line| line.contains("A.kt") && line.contains(":3:")),
        "expected parameter usage (A.kt:3:...) to survive --exclude-imports:\n{excluded_output}"
    );
}

#[test]
fn context_flag_prints_surrounding_source_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    write_fixture(root, "src/B.kt", "// header\nclass Foo\nval x = 1\n");

    let out_plain = Command::new(BIN)
        .args(["refs", "Foo", "--fast", "--root"])
        .arg(root)
        .output()
        .expect("spawn");
    let plain_output = String::from_utf8_lossy(&out_plain.stdout);
    // Without --context, no source text is printed at all.
    assert!(
        !plain_output.contains("// header"),
        "expected no source context without --context:\n{plain_output}"
    );

    let out_context = Command::new(BIN)
        .args(["refs", "Foo", "--fast", "--context", "1", "--root"])
        .arg(root)
        .output()
        .expect("spawn");
    let context_output = String::from_utf8_lossy(&out_context.stdout);
    assert!(
        context_output.contains("// header"),
        "expected line before match with --context 1:\n{context_output}"
    );
    assert!(
        context_output.contains("class Foo"),
        "expected the matched line itself with --context 1:\n{context_output}"
    );
    assert!(
        context_output.contains("val x = 1"),
        "expected line after match with --context 1:\n{context_output}"
    );
}

#[test]
fn context_flag_adds_context_array_to_json_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    write_fixture(root, "src/B.kt", "// header\nclass Foo\nval x = 1\n");

    let out = Command::new(BIN)
        .args([
            "refs",
            "Foo",
            "--fast",
            "--context",
            "1",
            "--json",
            "--root",
        ])
        .arg(root)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let entries = parsed.as_array().expect("array of results");
    let entry = entries
        .iter()
        .find(|e| e["file"].as_str().unwrap_or_default().contains("B.kt"))
        .expect("B.kt entry present");
    let context = entry["context"].as_array().expect("context array present");
    assert_eq!(context.len(), 3, "expected 3 context lines: {context:?}");
    assert_eq!(context[0]["line"], 1);
    assert_eq!(context[0]["text"], "// header");
    assert_eq!(context[1]["line"], 2);
    assert_eq!(context[1]["text"], "class Foo");
    assert_eq!(context[2]["line"], 3);
    assert_eq!(context[2]["text"], "val x = 1");
}

#[test]
fn context_flag_with_huge_n_does_not_overflow() {
    // Regression test: `result.line + n` must not panic/overflow when N is
    // near u32::MAX (previously used plain `+` instead of `saturating_add`).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    write_fixture(root, "src/B.kt", "// header\nclass Foo\nval x = 1\n");

    let out = Command::new(BIN)
        .args(["refs", "Foo", "--fast", "--context", "4294967295", "--root"])
        .arg(root)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "expected clean exit with huge --context, got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Context clamps to the whole (3-line) file rather than overflowing.
    assert!(stdout.contains("// header"));
    assert!(stdout.contains("val x = 1"));
}

/// Runs `refs --smart -v` (on an index built first) against a workspace whose Gradle cache holds one
/// dependency JAR, with `HOME` isolated so no real cache or sources leak in.
fn smart_refs_stderr(extra_args: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    let gradle_home = dir.path().join("gradle");
    write_fixture(&root, "workspace.json", r#"{"sourcePaths":[]}"#);
    write_fixture(&root, "src/A.kt", "class Foo\nfun useIt(f: Foo) {}\n");
    let jar =
        gradle_home.join("caches/modules-2/files-2.1/com.example/lib/1.0.0/abc123/lib-1.0.0.jar");
    std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
    std::fs::write(&jar, b"not a real jar").unwrap();

    // `--smart` needs a pre-built index.
    let index_status = Command::new(BIN)
        .args(["index", "--root"])
        .arg(&root)
        .env("HOME", dir.path())
        .env("GRADLE_USER_HOME", &gradle_home)
        .status()
        .expect("spawn");
    assert!(index_status.success(), "kmp-lsp index failed");

    let output = Command::new(BIN)
        .args(["refs", "Foo", "--smart", "-v"])
        .args(extra_args)
        .arg("--root")
        .arg(&root)
        .env("HOME", dir.path())
        .env("GRADLE_USER_HOME", &gradle_home)
        .env_remove("ANDROID_HOME")
        .env_remove("ANDROID_SDK_ROOT")
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("A.kt"),
        "expected the workspace reference either way:\n{stdout}"
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn smart_refs_loads_dependency_jars_by_default() {
    let stderr = smart_refs_stderr(&[]);
    assert!(
        stderr.contains("from 1 JAR(s)"),
        "expected refs to index the Gradle-cache JAR by default:\n{stderr}"
    );
}

#[test]
fn no_deps_flag_skips_dependency_jars() {
    let stderr = smart_refs_stderr(&["--no-deps"]);
    assert!(
        stderr.contains("from 0 JAR(s)"),
        "expected --no-deps to skip the Gradle-cache JAR:\n{stderr}"
    );
}

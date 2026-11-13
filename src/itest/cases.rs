//! The cases: one file each, under `cases/<language>/<kind>/`.
//!
//! A case file is a fixture ([`super::fixture`]) followed by the answer it
//! expects, separated by a `//= expected` line:
//!
//! ```text
//! //- /com/demo/Greeter.java
//! package com.demo;
//! class Greeter { String greet() { return ""; } }
//! //- /com/demo/Main.java
//! package com.demo;
//! class Main { void run(Greeter greeter) { greeter.gr$0eet(); } }
//! //= expected
//! /com/demo/Greeter.java:2:24  class Greeter { String greet() { return ""; } }
//! ```
//!
//! The directory a case sits in says which request is asked of it and which
//! language it is written in, so a new case is a new file and no Rust: the
//! set scales to thousands of them. One `#[test]` per language and kind runs
//! a whole directory, its cases in parallel, and names every file that
//! disagreed.
//!
//! A suite is off unless `INTEGRATION=1` is set: a case is a whole server,
//! and `cargo test` on its own stays a unit-test run.

use std::path::{Path, PathBuf};

use futures::stream::{self, StreamExt};

use super::fixture::Fixture;
use super::request::{Kind, Settles, NOTHING};
use super::server::Session;

const EXTENSION: &str = "case";
const SEPARATOR: &str = "//= expected";
const INTEGRATION_VARIABLE: &str = "INTEGRATION";
/// How many cases of one suite are in flight; what runs is capped by the
/// budget they share with every other suite.
const CASES_IN_FLIGHT: usize = 64;

/// One runtime for every suite, and a cap on how many cases run at once.
///
/// A case is a server of its own, and the test harness runs the suites
/// themselves in parallel: without a shared budget a thousand servers start
/// at once, each waits longer than its patience for a machine busy with the
/// others, and cases fail for want of a CPU rather than a wrong answer.
fn shared() -> &'static (tokio::runtime::Runtime, tokio::sync::Semaphore) {
    static SHARED: std::sync::OnceLock<(tokio::runtime::Runtime, tokio::sync::Semaphore)> =
        std::sync::OnceLock::new();
    SHARED.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a tokio runtime");
        let at_once = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        (runtime, tokio::sync::Semaphore::new(at_once))
    })
}

macro_rules! suites {
    ($($language:ident { $($case_test:ident => $kind:ident),* $(,)? })*) => {
        $(mod $language {
            use super::{suite, Kind};
            $(#[test]
            fn $case_test() {
                suite(stringify!($language), stringify!($case_test), Kind::$kind);
            })*
        })*

        /// Every `<language>/<request>` a suite above runs.
        const RUN_DIRECTORIES: &[(&str, &str)] = &[
            $($((stringify!($language), stringify!($case_test)),)*)*
        ];
    };
}

suites! {
    java {
        definition => Definition,
        type_definition => TypeDefinition,
        references => References,
        document_highlight => DocumentHighlight,
        completion => Completion,
        hover => Hover,
        signature_help => SignatureHelp,
        prepare_rename => PrepareRename,
        document_symbol => DocumentSymbol,
        folding_range => FoldingRange,
        workspace_symbol => WorkspaceSymbol,
    }
    kotlin {
        definition => Definition,
        type_definition => TypeDefinition,
        references => References,
        document_highlight => DocumentHighlight,
        completion => Completion,
        hover => Hover,
        signature_help => SignatureHelp,
        prepare_rename => PrepareRename,
        document_symbol => DocumentSymbol,
        folding_range => FoldingRange,
        workspace_symbol => WorkspaceSymbol,
    }
}

/// A case directory no suite runs would hold cases nothing asks: the
/// `suites!` list above and the `cases/` tree have to say the same thing.
#[test]
fn every_case_directory_is_run_by_a_suite() {
    let mut unrun = Vec::new();
    for language in read_directory(&cases_root()) {
        for request in read_directory(&language) {
            let pair = (directory_name(&language), directory_name(&request));
            let listed = RUN_DIRECTORIES.iter().any(|(run_language, run_request)| {
                (*run_language, *run_request) == (&pair.0[..], &pair.1[..])
            });
            if !listed {
                unrun.push(format!("{}/{}", pair.0, pair.1));
            }
        }
    }
    assert!(
        unrun.is_empty(),
        "no suite asks these case directories: {}\nAdd them to `suites!` in {}.",
        unrun.join(", "),
        file!()
    );
}

/// Asks `kind` of every case in `cases/<language>/<kind_directory>` and
/// fails with the ones whose answer is not what their file expects.
fn suite(language: &str, kind_directory: &str, kind: Kind) {
    if !requested() {
        eprintln!(
            "{language}/{kind_directory}: skipped, \
             set {INTEGRATION_VARIABLE}=1 to ask the cases"
        );
        return;
    }
    let directory = cases_root().join(language).join(kind_directory);
    let files = case_files(&directory);
    assert!(
        !files.is_empty(),
        "{} holds no .{EXTENSION} file",
        directory.display()
    );
    let (runtime, at_once) = shared();
    let disagreements: Vec<String> = runtime.block_on(
        stream::iter(files)
            .map(|file| async move {
                let _running = at_once.acquire().await.expect("the budget is never closed");
                run_case(&file, kind).await
            })
            .buffer_unordered(CASES_IN_FLIGHT)
            .filter_map(|outcome| async move { outcome })
            .collect(),
    );
    assert!(
        disagreements.is_empty(),
        "{} of {language}/{kind_directory} answered differently:\n\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}

/// `None` when the case passes, otherwise what it answered instead.
async fn run_case(file: &Path, kind: Kind) -> Option<String> {
    let case = Case::read(file);
    let fixture = Fixture::parse(&case.fixture);
    let mut session = Session::open(&fixture).await;
    let actual = kind
        .ask(&mut session, &fixture, &case.expected, case.settles())
        .await;
    if actual == case.expected {
        return None;
    }
    Some(format!(
        "{}\n  expected:\n{}  answered:\n{}",
        file.display(),
        indented(&case.expected),
        indented(&actual)
    ))
}

/// Whether the cases were asked for: a suite starts a server per case, so
/// it runs only under `INTEGRATION=1` and otherwise says why it did
/// nothing.
fn requested() -> bool {
    asked_for(INTEGRATION_VARIABLE)
}

fn asked_for(variable: &str) -> bool {
    std::env::var(variable).is_ok_and(|value| value != "0")
}

/// A case file split at its `//= expected` line.
struct Case {
    fixture: String,
    expected: String,
}

impl Case {
    fn read(file: &Path) -> Self {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("{} is readable: {error}", file.display()));
        let (fixture, expected) = text.split_once(SEPARATOR).unwrap_or_else(|| {
            panic!("{} has a `{SEPARATOR}` line", file.display());
        });
        Self {
            fixture: fixture.to_owned(),
            expected: normalized(expected),
        }
    }

    /// A case whose answer is nothing is asked once; any other waits for
    /// the server to have something to say.
    fn settles(&self) -> Settles {
        if self.expected.trim() == NOTHING {
            Settles::Immediately
        } else {
            Settles::WhenAnswered
        }
    }
}

/// An expectation as [`Kind::ask`] renders an answer: lines with no blank
/// ones around them, ending in one newline. An empty one stays empty, so a
/// case file committed without an answer disagrees with every answer.
fn normalized(expected: &str) -> String {
    let body = expected.trim();
    if body.is_empty() {
        return String::new();
    }
    body.to_owned() + "\n"
}

fn indented(text: &str) -> String {
    if text.is_empty() {
        return "    (nothing written down yet)\n".to_owned();
    }
    text.lines()
        .map(|line| format!("    {line}\n"))
        .collect::<String>()
}

fn cases_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/itest/cases")
}

/// The subdirectories of `directory`, in name order.
fn read_directory(directory: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{} is a directory: {error}", directory.display()));
    let mut directories: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let path = entry.expect("a readable directory entry").path();
            path.is_dir().then_some(path)
        })
        .collect();
    directories.sort();
    directories
}

fn directory_name(directory: &Path) -> String {
    directory
        .file_name()
        .expect("a directory has a name")
        .to_string_lossy()
        .into_owned()
}

/// The case files of `directory`, in name order, so a failure list reads
/// the same on every machine.
fn case_files(directory: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{} is a directory: {error}", directory.display()));
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let path = entry.expect("a readable directory entry").path();
            let is_case = path.extension().is_some_and(|suffix| suffix == EXTENSION);
            is_case.then_some(path)
        })
        .collect();
    files.sort();
    files
}

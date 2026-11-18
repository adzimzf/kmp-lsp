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
//! A Java case whose answer depends on the Java release the project is
//! compiled for splits its expectation by release, one block per range:
//!
//! ```text
//! //- /src/main/java/com/demo/Point.java
//! package com.demo;
//!
//! record Po$0int(int x, int y) {}
//! //= expected java ..15
//! /src/main/java/com/demo/Point.java:3:1  records need Java 16 or later
//! //= expected java 16..
//! (nothing)
//! ```
//!
//! A range is `8..15`, `..15` (from [`OLDEST_JAVA`]), `16..` (up to
//! [`NEWEST_JAVA`]) or one release, `21`. The ranges of a case cover every
//! release from the oldest to the newest exactly once, so no release goes
//! unasked by accident. Each range is asked at both its ends, where an
//! off-by-one in a feature's release shows; the server is told the release
//! as `indexingOptions.javaRelease`. A plain `//= expected` holds for every
//! release and is asked once, without the setting.
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
/// What follows [`SEPARATOR`] when the expectation holds for some Java
/// releases only.
const JAVA_RELEASES: &str = "java ";
/// The oldest and newest releases a range can name: `..15` starts at the
/// one, `16..` ends at the other.
pub(super) const OLDEST_JAVA: u8 = 8;
pub(super) const NEWEST_JAVA: u8 = 25;
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
        implementation => Implementation,
        references => References,
        document_highlight => DocumentHighlight,
        completion => Completion,
        hover => Hover,
        signature_help => SignatureHelp,
        prepare_rename => PrepareRename,
        rename => Rename,
        document_symbol => DocumentSymbol,
        folding_range => FoldingRange,
        workspace_symbol => WorkspaceSymbol,
        diagnostics => Diagnostics,
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

/// Every case file splits into a fixture and expectations that cover
/// every Java release: checked without `INTEGRATION=1`, so a bad range is
/// caught by a plain `cargo test`.
#[test]
fn every_case_file_reads() {
    let mut broken = Vec::new();
    for language in read_directory(&cases_root()) {
        for request in read_directory(&language) {
            for file in case_files(&request) {
                let text = std::fs::read_to_string(&file).expect("a readable case");
                if let Err(problem) = Case::parse(&text) {
                    broken.push(format!("{}: {problem}", file.display()));
                }
            }
        }
    }
    assert!(broken.is_empty(), "{}", broken.join("\n"));
}

#[test]
fn releases_name_a_range_or_one_release() {
    assert_eq!(Releases::parse(""), Ok(Releases::Every));
    assert_eq!(
        Releases::parse("java ..15"),
        Ok(Releases::Range(OLDEST_JAVA, 15))
    );
    assert_eq!(
        Releases::parse("java 16.."),
        Ok(Releases::Range(16, NEWEST_JAVA))
    );
    assert_eq!(Releases::parse("java 14..15"), Ok(Releases::Range(14, 15)));
    assert_eq!(Releases::parse("java 21"), Ok(Releases::Range(21, 21)));
    assert!(Releases::parse("java 7..").is_err());
    assert!(Releases::parse("java 16..15").is_err());
    assert!(Releases::parse("jdk 16").is_err());
}

#[test]
fn ranges_have_to_cover_every_release_once() {
    let case = |text: &str| Case::parse(&format!("//- /A.java\nclass A$0 {{}}\n{text}"));
    assert!(case("//= expected java ..15\n(nothing)\n//= expected java 16..\n(nothing)\n").is_ok());
    assert!(
        case("//= expected java ..14\n(nothing)\n//= expected java 16..\n(nothing)\n").is_err()
    );
    assert!(
        case("//= expected java ..16\n(nothing)\n//= expected java 16..\n(nothing)\n").is_err()
    );
    assert!(case("//= expected\n(nothing)\n//= expected java 16..\n(nothing)\n").is_err());
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

/// `None` when the case passes, otherwise what it answered instead, for
/// every release it answered differently at.
async fn run_case(file: &Path, kind: Kind) -> Option<String> {
    let case = Case::read(file);
    let fixture = Fixture::parse(&case.fixture);
    let mut disagreements = Vec::new();
    for expectation in &case.expectations {
        for release in expectation.releases.asked_at() {
            let mut session = Session::open(&fixture, release).await;
            let actual = kind
                .ask(
                    &mut session,
                    &fixture,
                    &expectation.expected,
                    expectation.settles(),
                )
                .await;
            if actual == expectation.expected {
                continue;
            }
            let at = release.map_or_else(String::new, |release| format!(" (java {release})"));
            disagreements.push(format!(
                "{}{at}\n  expected:\n{}  answered:\n{}",
                file.display(),
                indented(&expectation.expected),
                indented(&actual)
            ));
        }
    }
    (!disagreements.is_empty()).then(|| disagreements.join("\n"))
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

/// A case file split at its `//= expected` lines.
struct Case {
    fixture: String,
    expectations: Vec<Expectation>,
}

/// The answer a case expects at some Java releases.
struct Expectation {
    releases: Releases,
    expected: String,
}

/// Which Java releases an expectation holds for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Releases {
    /// Every release: the answer does not depend on it.
    Every,
    /// `first..=last`.
    Range(u8, u8),
}

impl Case {
    fn read(file: &Path) -> Self {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("{} is readable: {error}", file.display()));
        Self::parse(&text).unwrap_or_else(|problem| panic!("{}: {problem}", file.display()))
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut blocks = text.split(SEPARATOR);
        let fixture = blocks.next().unwrap_or_default().to_owned();
        let mut expectations = Vec::new();
        for block in blocks {
            let (header, body) = block.split_once('\n').unwrap_or((block, ""));
            expectations.push(Expectation {
                releases: Releases::parse(header.trim())?,
                expected: normalized(body),
            });
        }
        if expectations.is_empty() {
            return Err(format!("no `{SEPARATOR}` line"));
        }
        check_cover(&expectations)?;
        Ok(Self {
            fixture,
            expectations,
        })
    }
}

/// One `//= expected` for every release, or ranges that name each release
/// from [`OLDEST_JAVA`] to [`NEWEST_JAVA`] exactly once.
fn check_cover(expectations: &[Expectation]) -> Result<(), String> {
    let every = expectations
        .iter()
        .filter(|expectation| expectation.releases == Releases::Every)
        .count();
    if every > 0 {
        return if expectations.len() == 1 {
            Ok(())
        } else {
            Err("a plain `//= expected` already holds for every release".to_owned())
        };
    }
    for release in OLDEST_JAVA..=NEWEST_JAVA {
        let naming = expectations
            .iter()
            .filter(|expectation| expectation.releases.contains(release))
            .count();
        match naming {
            1 => {}
            0 => return Err(format!("no expectation for java {release}")),
            _ => return Err(format!("java {release} is expected {naming} times")),
        }
    }
    Ok(())
}

impl Releases {
    /// What follows `//= expected`: nothing, or `java <range>`.
    fn parse(header: &str) -> Result<Self, String> {
        if header.is_empty() {
            return Ok(Self::Every);
        }
        let range = header
            .strip_prefix(JAVA_RELEASES)
            .ok_or_else(|| format!("`{SEPARATOR} {header}`: expected `{JAVA_RELEASES}<range>`"))?
            .trim();
        let release = |text: &str, otherwise: u8| -> Result<u8, String> {
            if text.is_empty() {
                return Ok(otherwise);
            }
            let release: u8 = text
                .parse()
                .map_err(|_| format!("`{text}` is no Java release"))?;
            if !(OLDEST_JAVA..=NEWEST_JAVA).contains(&release) {
                return Err(format!(
                    "java {release} is outside {OLDEST_JAVA}..{NEWEST_JAVA}"
                ));
            }
            Ok(release)
        };
        let (first, last) = match range.split_once("..") {
            Some((first, last)) => (release(first, OLDEST_JAVA)?, release(last, NEWEST_JAVA)?),
            None if range.is_empty() => return Err("`java` without a release".to_owned()),
            None => {
                let only = release(range, OLDEST_JAVA)?;
                (only, only)
            }
        };
        if first > last {
            return Err(format!("`{range}` runs backwards"));
        }
        Ok(Self::Range(first, last))
    }

    fn contains(self, release: u8) -> bool {
        match self {
            Self::Every => true,
            Self::Range(first, last) => (first..=last).contains(&release),
        }
    }

    /// The releases the server is told, `None` for no setting at all.
    fn asked_at(self) -> Vec<Option<u8>> {
        match self {
            Self::Every => vec![None],
            Self::Range(first, last) if first == last => vec![Some(first)],
            Self::Range(first, last) => vec![Some(first), Some(last)],
        }
    }
}

impl Expectation {
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

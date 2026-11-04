//! Find-references into dependency sources, over the real `kmp-lsp --stdio`
//! server.
//!
//! The `app` fixture (Kotlin + Java) depends on the `textkit` library, whose
//! real sources sit in `textkit-1.0.0-sources.jar` in the Gradle cache —
//! exactly where Gradle (or the server's dependency sync) leaves a downloaded
//! sources jar. `textkit` uses `Normalizer.normalize` itself, from Kotlin
//! (`Slugger`) and from Java through the object's `INSTANCE` field
//! (`LegacyFormatter`), and declares a decoy `Whitespace.normalize` that is
//! never a reference.
//!
//! Library hits come back as `file://` copies extracted from the jar, so an
//! editor can open them.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const APP_KT: &str = "src/main/kotlin/com/acme/app/App.kt";
const JAVA_APP_JAVA: &str = "src/main/java/com/acme/app/JavaApp.java";
/// Library sources are mounted by the background JAR phase, which finishes
/// after the workspace-indexing progress ends.
const LIBRARY_READY_TIMEOUT: Duration = Duration::from_secs(60);

struct LibraryWorkspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// The server's `XDG_CACHE_HOME`.
    cache_home: PathBuf,
    client: LspClient,
}

impl LibraryWorkspace {
    fn start(initialization_options: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = canonical_root(dir.path());
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        copy_dir(&textkit::fixture_dir().join("app"), &root);
        write(&root, "workspace.json", r#"{"sourcePaths":[]}"#);

        let gradle_user_home = base.join("gradle-home");
        textkit::install_sources_in_gradle_cache(&gradle_user_home);
        let environment = ServerEnvironment {
            gradle_user_home: Some(gradle_user_home),
            variables: vec![("XDG_CACHE_HOME".to_owned(), base.join("xdg-cache"))],
        };

        let mut client = LspClient::spawn_with(&root, &environment);
        client.initialize_with_options(&root, initialization_options);
        client.wait_for_indexing();
        LibraryWorkspace {
            _dir: dir,
            root,
            cache_home: base.join("xdg-cache"),
            client,
        }
    }

    fn open(&mut self, relative_path: &str) -> (String, String) {
        let uri = file_uri(&self.root, relative_path);
        let text = std::fs::read_to_string(self.root.join(relative_path)).unwrap();
        let language = if relative_path.ends_with(".java") {
            "java"
        } else {
            "kotlin"
        };
        self.client.open_file(&uri, language, &text);
        (uri, text)
    }

    fn request_locations(&mut self, method: &str, uri: &str, at: (u64, u64)) -> Vec<(String, u64)> {
        let mut params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": at.0, "character": at.1},
        });
        if method == "textDocument/references" {
            params["context"] = json!({"includeDeclaration": false});
        }
        let response = self.client.request(method, params);
        locations(&response["result"])
    }

    /// Labels offered by completion at `at`, re-asked until `accept` passes.
    fn poll_completion(
        &mut self,
        uri: &str,
        at: (u64, u64),
        accept: impl Fn(&[String]) -> bool,
    ) -> Vec<String> {
        let deadline = Instant::now() + LIBRARY_READY_TIMEOUT;
        loop {
            let response = self.client.request(
                "textDocument/completion",
                json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": at.0, "character": at.1},
                }),
            );
            let result = &response["result"];
            let items = result
                .as_array()
                .or_else(|| result["items"].as_array())
                .cloned()
                .unwrap_or_default();
            let labels: Vec<String> = items
                .iter()
                .filter_map(|item| item["label"].as_str().map(str::to_owned))
                .collect();
            if accept(&labels) || Instant::now() > deadline {
                return labels;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Files in the server's cache directory whose path contains `needle`.
    fn cache_files_containing(&self, needle: &str) -> Vec<PathBuf> {
        fn walk(directory: &std::path::Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found);
                } else {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.cache_home, &mut found);
        found
            .into_iter()
            .filter(|path| path.to_string_lossy().contains(needle))
            .collect()
    }

    /// Re-ask `method` until `accept` passes — the library is mounted
    /// asynchronously after indexing ends.
    fn poll_locations(
        &mut self,
        method: &str,
        uri: &str,
        at: (u64, u64),
        accept: impl Fn(&[(String, u64)]) -> bool,
    ) -> Vec<(String, u64)> {
        let deadline = Instant::now() + LIBRARY_READY_TIMEOUT;
        loop {
            let found = self.request_locations(method, uri, at);
            if accept(&found) || Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

/// 0-based `(line, column)` of `token` on the first line containing `needle`.
fn spot(text: &str, needle: &str, token: &str) -> (u64, u64) {
    let (line_index, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .unwrap_or_else(|| panic!("no line contains {needle:?}"));
    let column =
        line.find(needle).unwrap() + line[line.find(needle).unwrap()..].find(token).unwrap();
    (
        line_index as u64,
        line[..column].encode_utf16().count() as u64,
    )
}

/// The text of `line` in the file a `file://` result points at.
fn line_at(uri: &str, line: u64) -> String {
    let path = tower_lsp::lsp_types::Url::parse(uri)
        .unwrap()
        .to_file_path()
        .unwrap_or_else(|_| panic!("{uri} is not a file:// location an editor can open"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} unreadable: {error}", path.display()));
    text.lines()
        .nth(line as usize)
        .unwrap_or_default()
        .to_owned()
}

fn file_name(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

/// Results located in a file named `name`, with each one's line text.
fn hits_in(found: &[(String, u64)], name: &str) -> Vec<String> {
    found
        .iter()
        .filter(|(uri, _)| file_name(uri) == name)
        .map(|(uri, line)| line_at(uri, *line))
        .collect()
}

fn includes_library_usages(found: &[(String, u64)]) -> bool {
    !hits_in(found, "Slugger.kt").is_empty() && !hits_in(found, "LegacyFormatter.java").is_empty()
}

fn assert_library_usages(found: &[(String, u64)]) {
    let slugger = hits_in(found, "Slugger.kt");
    assert_eq!(
        slugger.len(),
        1,
        "Slugger.kt hits: {slugger:?}; all: {found:?}"
    );
    assert!(
        slugger[0].contains("Normalizer.normalize(title)"),
        "{slugger:?}"
    );

    let formatter = hits_in(found, "LegacyFormatter.java");
    assert_eq!(
        formatter.len(),
        1,
        "LegacyFormatter.java hits: {formatter:?}; all: {found:?}"
    );
    assert!(
        formatter[0].contains("Normalizer.INSTANCE.normalize(text)"),
        "{formatter:?}"
    );

    // `Whitespace.normalize` is a different function with the same name.
    let decoys: Vec<&(String, u64)> = found
        .iter()
        .filter(|(uri, line)| {
            let is_library_file =
                file_name(uri) == "Normalizer.kt" || file_name(uri) == "Whitespace.kt";
            is_library_file && line_at(uri, *line).contains("Whitespace")
        })
        .collect();
    assert!(
        decoys.is_empty(),
        "Whitespace.normalize is not a reference: {decoys:?}"
    );
    for (uri, _) in found {
        assert!(
            uri.starts_with("file://"),
            "every location must be openable by an editor: {uri}"
        );
    }
}

#[test]
fn references_from_app_usage_include_usages_inside_library_sources() {
    let mut workspace = LibraryWorkspace::start(Value::Null);
    let (app_uri, app_text) = workspace.open(APP_KT);
    let at = spot(&app_text, "Normalizer.normalize(title)", "normalize");

    let found = workspace.poll_locations(
        "textDocument/references",
        &app_uri,
        at,
        includes_library_usages,
    );

    assert_library_usages(&found);
    let java_app = file_uri(&workspace.root, JAVA_APP_JAVA);
    assert!(
        found.iter().any(|(uri, _)| *uri == java_app),
        "the workspace's Java usage stays included: {found:?}"
    );
}

#[test]
fn references_from_library_declaration_include_app_and_library_usages() {
    let mut workspace = LibraryWorkspace::start(Value::Null);
    let (app_uri, app_text) = workspace.open(APP_KT);
    let at = spot(&app_text, "Normalizer.normalize(title)", "normalize");

    // Go to the declaration inside the library, as a user would.
    let definition = workspace.poll_locations("textDocument/definition", &app_uri, at, |found| {
        found
            .iter()
            .any(|(uri, _)| file_name(uri) == "Normalizer.kt")
    });
    let (library_uri, declaration_line) = definition
        .iter()
        .find(|(uri, _)| file_name(uri) == "Normalizer.kt")
        .cloned()
        .unwrap_or_else(|| panic!("definition never reached the library: {definition:?}"));
    let declaration_text = line_at(&library_uri, declaration_line);
    assert!(
        declaration_text.contains("fun normalize("),
        "{declaration_text}"
    );
    let column = declaration_text.find("normalize").unwrap() as u64;

    let found = workspace.request_locations(
        "textDocument/references",
        &library_uri,
        (declaration_line, column),
    );

    assert_library_usages(&found);
    assert!(
        found.iter().any(|(uri, _)| *uri == app_uri),
        "the app's usage must be included: {found:?}"
    );
}

#[test]
fn library_references_can_be_turned_off() {
    let mut workspace =
        LibraryWorkspace::start(json!({"indexingOptions": {"referencesIncludeLibraries": false}}));
    let (app_uri, app_text) = workspace.open(APP_KT);
    let at = spot(&app_text, "Normalizer.normalize(title)", "normalize");

    // Wait until the library is mounted (definition reaches it), so the
    // absence of library references below isn't just a race.
    let definition = workspace.poll_locations("textDocument/definition", &app_uri, at, |found| {
        found
            .iter()
            .any(|(uri, _)| file_name(uri) == "Normalizer.kt")
    });
    assert!(
        definition
            .iter()
            .any(|(uri, _)| file_name(uri) == "Normalizer.kt"),
        "library never mounted: {definition:?}"
    );

    let found = workspace.request_locations("textDocument/references", &app_uri, at);
    assert!(
        hits_in(&found, "Slugger.kt").is_empty()
            && hits_in(&found, "LegacyFormatter.java").is_empty(),
        "library usages must be left out when turned off: {found:?}"
    );
}

/// Library sources load on demand by default: only names are known up front,
/// and a library is read from its own cache file when first used.
#[test]
fn library_sources_load_on_demand_from_a_per_library_cache() {
    let mut workspace = LibraryWorkspace::start(Value::Null);
    let (app_uri, app_text) = workspace.open(APP_KT);
    let at = spot(&app_text, "Normalizer.normalize(title)", "normalize");

    let definition = workspace.poll_locations("textDocument/definition", &app_uri, at, |found| {
        found
            .iter()
            .any(|(uri, _)| file_name(uri) == "Normalizer.kt")
    });
    assert!(
        definition
            .iter()
            .any(|(uri, _)| file_name(uri) == "Normalizer.kt"),
        "definition must reach the library: {definition:?}"
    );

    let cached = workspace.cache_files_containing("textkit-1.0.0-sources");
    let has = |extension: &str| {
        cached.iter().any(|path| {
            path.to_string_lossy().contains("library-sources")
                && path.extension().is_some_and(|e| e == extension)
        })
    };
    assert!(
        has("index") && has("parse"),
        "per-library cache files: {cached:?}"
    );
    assert!(
        workspace.cache_files_containing("sources-jar-").is_empty(),
        "the eager all-libraries cache is not written"
    );
}

/// Completion offers a library's names before the library is loaded, and its
/// members once a file uses it.
#[test]
fn completion_works_for_libraries_loaded_on_demand() {
    let mut workspace = LibraryWorkspace::start(Value::Null);
    let relative_path = "src/main/kotlin/com/acme/app/Draft.kt";
    let uri = file_uri(&workspace.root, relative_path);
    let bare = "package com.acme.app\n\nfun draft() {\n    Whitesp\n}\n";
    workspace.client.open_file(&uri, "kotlin", bare);

    let labels = workspace.poll_completion(&uri, (3, 11), |labels| {
        labels.iter().any(|label| label == "Whitespace")
    });
    assert!(
        labels.iter().any(|label| label == "Whitespace"),
        "a library class is offered by name: {labels:?}"
    );

    let dotted =
        "package com.acme.app\n\nimport com.acme.textkit.Whitespace\n\nfun draft() {\n    Whitespace.\n}\n";
    workspace.client.change_file(&uri, 2, dotted);
    let labels = workspace.poll_completion(&uri, (5, 15), |labels| {
        labels.iter().any(|label| label == "normalize")
    });
    assert!(
        labels.iter().any(|label| label == "normalize"),
        "members of a library type: {labels:?}"
    );
}

/// `librarySources: "eager"` keeps loading every library at startup.
#[test]
fn library_sources_can_be_loaded_eagerly() {
    let mut workspace =
        LibraryWorkspace::start(json!({"indexingOptions": {"librarySources": "eager"}}));
    let (app_uri, app_text) = workspace.open(APP_KT);
    let at = spot(&app_text, "Normalizer.normalize(title)", "normalize");

    let found = workspace.poll_locations(
        "textDocument/references",
        &app_uri,
        at,
        includes_library_usages,
    );

    assert_library_usages(&found);
    assert!(
        !workspace.cache_files_containing("sources-jar-").is_empty(),
        "eager mode uses the all-libraries cache"
    );
    assert!(
        workspace
            .cache_files_containing("library-sources")
            .is_empty(),
        "and never the per-library one"
    );
}

//! Automatic dependency sync over the real `kmp-lsp --stdio` server.
//!
//! The server asks the project's own build tool for its dependencies — Gradle
//! through an init script, Maven through `maven-dependency-plugin` — which
//! DOWNLOADS each dependency and its sources jar, then indexes them so
//! go-to-definition and find-references reach into the library.
//!
//! The `textkit` fixture library is published to a private repository (over
//! HTTP for Gradle, `file://` for Maven) that starts out absent from the
//! build tool's cache, so a passing test proves the download happened.
//!
//! Gradle/Maven tests are skipped (with a message) when the tool isn't on
//! `PATH`; Maven additionally needs Maven Central for its own plugin.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const SYNC_TOKEN: &str = "kmp-lsp/dependency-sync";
const SYNC_TIMEOUT: Duration = Duration::from_secs(300);
const LIBRARY_READY_TIMEOUT: Duration = Duration::from_secs(60);
const APP_KT: &str = "src/main/kotlin/com/acme/app/App.kt";

fn tool_available(tool: &str) -> bool {
    let available = Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        eprintln!("skipping: `{tool}` is not on PATH");
    }
    available
}

/// Serve `root` over HTTP on 127.0.0.1 (GET/HEAD, 404 when missing) for the
/// life of the test process. Returns the base URL.
fn serve_directory(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("/").trim_start_matches('/');
                let file = root.join(path);
                let mut stream = stream;
                let body = (!path.contains(".."))
                    .then(|| std::fs::read(&file).ok())
                    .flatten();
                let _ = match body {
                    Some(bytes) => {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            bytes.len()
                        );
                        stream.write_all(header.as_bytes()).and_then(|_| {
                            if method == "HEAD" {
                                Ok(())
                            } else {
                                stream.write_all(&bytes)
                            }
                        })
                    }
                    None => stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    ),
                };
            });
        }
    });
    format!("http://{address}/")
}

const WELCOME_KT: &str = "src/main/kotlin/com/acme/app/Welcome.kt";

struct SyncWorkspace {
    _dir: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    environment: ServerEnvironment,
}

impl SyncWorkspace {
    /// The `app` fixture plus `build_files`, with an isolated Gradle home,
    /// Maven repository and cache dir.
    fn new(build_files: &[(&str, String)]) -> Self {
        Self::from_fixture("app", build_files)
    }

    /// `fixture` (a directory under `tests/fixtures/library-refs`) plus
    /// `build_files`, with an isolated Gradle home, Maven repository and
    /// cache dir.
    fn from_fixture(fixture: &str, build_files: &[(&str, String)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = canonical_root(dir.path());
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        copy_dir(&textkit::fixture_dir().join(fixture), &root);
        write(&root, "workspace.json", r#"{"sourcePaths":[]}"#);
        for (name, content) in build_files {
            write(&root, name, content);
        }
        let gradle_user_home = base.join("gradle-home");
        // One-shot JVMs: no daemon left running after the test.
        write(
            &gradle_user_home,
            "gradle.properties",
            "org.gradle.daemon=false\n",
        );
        let environment = ServerEnvironment {
            gradle_user_home: Some(gradle_user_home),
            variables: vec![
                ("XDG_CACHE_HOME".to_owned(), base.join("xdg-cache")),
                (
                    "MAVEN_OPTS".to_owned(),
                    PathBuf::from(format!("-Dmaven.repo.local={}", base.join("m2").display())),
                ),
            ],
        };
        SyncWorkspace {
            _dir: dir,
            base,
            root,
            environment,
        }
    }

    fn start(&self, initialization_options: Value) -> LspClient {
        let mut client = LspClient::spawn_with(&self.root, &self.environment);
        client.initialize_with_options(&self.root, initialization_options);
        client.wait_for_indexing();
        client
    }

    fn gradle_cache(&self) -> PathBuf {
        self.base.join("gradle-home/caches/modules-2/files-2.1")
    }

    fn app_uri(&self) -> String {
        file_uri(&self.root, APP_KT)
    }

    /// `(line, column)` of `normalize` in `Normalizer.normalize(title)`.
    fn normalize_call(&self) -> (u64, u64) {
        let text = std::fs::read_to_string(self.root.join(APP_KT)).unwrap();
        let (line_index, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Normalizer.normalize(title)"))
            .unwrap();
        (line_index as u64, line.find("normalize(").unwrap() as u64)
    }
}

fn open_app(client: &mut LspClient, workspace: &SyncWorkspace) {
    let text = std::fs::read_to_string(workspace.root.join(APP_KT)).unwrap();
    client.open_file(&workspace.app_uri(), "kotlin", &text);
}

fn definition(client: &mut LspClient, uri: &str, at: (u64, u64)) -> Vec<(String, u64)> {
    let response = client.request(
        "textDocument/definition",
        json!({"textDocument": {"uri": uri}, "position": {"line": at.0, "character": at.1}}),
    );
    locations(&response["result"])
}

fn reaches_library(found: &[(String, u64)]) -> bool {
    found.iter().any(|(uri, _)| uri.ends_with("/Normalizer.kt"))
}

/// Re-ask go-to-definition until it lands in the library's `Normalizer.kt`.
fn wait_for_library_definition(
    client: &mut LspClient,
    workspace: &SyncWorkspace,
) -> Vec<(String, u64)> {
    let deadline = Instant::now() + LIBRARY_READY_TIMEOUT;
    loop {
        let found = definition(client, &workspace.app_uri(), workspace.normalize_call());
        if reaches_library(&found) || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn find_file(directory: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if entry.file_name() == name {
            return Some(path);
        }
    }
    None
}

fn gradle_build(repository_url: &str, with_textkit: bool) -> String {
    let dependency = if with_textkit {
        "dependencies { implementation(\"com.acme:textkit:1.0.0\") }\n"
    } else {
        ""
    };
    format!(
        "plugins {{ `java-library` }}\n\
         repositories {{\n    maven {{\n        url = uri(\"{repository_url}\")\n        isAllowInsecureProtocol = true\n    }}\n}}\n\
         {dependency}"
    )
}

#[test]
fn gradle_sync_downloads_sources_and_resyncs_when_the_build_file_changes() {
    if !tool_available("gradle") {
        return;
    }
    let repository = tempfile::tempdir().unwrap();
    textkit::publish_to_maven_repository(repository.path());
    let repository_url = serve_directory(repository.path().to_path_buf());

    let workspace = SyncWorkspace::new(&[
        (
            "settings.gradle.kts",
            "rootProject.name = \"app\"\n".to_owned(),
        ),
        ("build.gradle.kts", gradle_build(&repository_url, false)),
    ]);
    let mut client = workspace.start(Value::Null);
    open_app(&mut client, &workspace);

    // First sync: no dependency yet, so nothing reaches the library.
    assert!(
        client.wait_for_progress(SYNC_TOKEN, "end", 1, SYNC_TIMEOUT),
        "initial dependency sync never finished"
    );
    let before = definition(
        &mut client,
        &workspace.app_uri(),
        workspace.normalize_call(),
    );
    assert!(
        !reaches_library(&before),
        "no dependency declared yet: {before:?}"
    );

    // Add the dependency, as an editor saving build.gradle.kts would.
    write(
        &workspace.root,
        "build.gradle.kts",
        &gradle_build(&repository_url, true),
    );
    let build_file_uri = file_uri(&workspace.root, "build.gradle.kts");
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": build_file_uri, "type": 2}]}),
    );
    assert!(
        client.wait_for_progress(SYNC_TOKEN, "end", 2, SYNC_TIMEOUT),
        "dependency sync after the build-file change never finished"
    );

    let sources_jar = find_file(&workspace.gradle_cache(), "textkit-1.0.0-sources.jar");
    assert!(
        sources_jar.is_some(),
        "Gradle must have downloaded the sources jar into its cache"
    );
    let after = wait_for_library_definition(&mut client, &workspace);
    assert!(
        reaches_library(&after),
        "go-to-definition must reach the downloaded library source: {after:?}"
    );
    drop(client);

    // A restart with unchanged build files reuses the saved result: no new
    // build-tool run, and the library is still indexed.
    let mut restarted = workspace.start(Value::Null);
    open_app(&mut restarted, &workspace);
    let found = wait_for_library_definition(&mut restarted, &workspace);
    assert!(reaches_library(&found), "after restart: {found:?}");
    assert_eq!(
        restarted.progress_count(SYNC_TOKEN, "begin"),
        0,
        "unchanged build files must not re-run the build tool"
    );
}

#[test]
fn maven_sync_downloads_sources_into_the_local_repository() {
    if !tool_available("mvn") {
        return;
    }
    let repository = tempfile::tempdir().unwrap();
    textkit::publish_to_maven_repository(repository.path());
    let repository_url = tower_lsp::lsp_types::Url::from_directory_path(repository.path())
        .unwrap()
        .to_string();
    let pom = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n\
         <modelVersion>4.0.0</modelVersion>\n\
         <groupId>com.acme</groupId><artifactId>app</artifactId><version>1.0.0</version>\n\
         <repositories><repository><id>textkit-fixture</id><url>{repository_url}</url></repository></repositories>\n\
         <dependencies><dependency><groupId>com.acme</groupId><artifactId>textkit</artifactId><version>1.0.0</version></dependency></dependencies>\n\
         </project>\n"
    );
    let workspace = SyncWorkspace::new(&[("pom.xml", pom)]);
    let mut client = workspace.start(Value::Null);
    open_app(&mut client, &workspace);

    assert!(
        client.wait_for_progress(SYNC_TOKEN, "end", 1, SYNC_TIMEOUT),
        "Maven dependency sync never finished"
    );
    let downloaded = workspace
        .base
        .join("m2/com/acme/textkit/1.0.0/textkit-1.0.0-sources.jar");
    assert!(
        downloaded.is_file(),
        "Maven must have downloaded the sources jar into the local repository"
    );
    let found = wait_for_library_definition(&mut client, &workspace);
    assert!(
        reaches_library(&found),
        "go-to-definition must reach the downloaded library source: {found:?}"
    );
}

#[test]
fn dependency_sync_off_never_runs_the_build_tool() {
    let workspace = SyncWorkspace::new(&[
        (
            "settings.gradle.kts",
            "rootProject.name = \"app\"\n".to_owned(),
        ),
        (
            "build.gradle.kts",
            gradle_build("http://127.0.0.1:9/", true),
        ),
    ]);
    let mut client = workspace.start(json!({"indexingOptions": {"dependencySync": "off"}}));
    open_app(&mut client, &workspace);

    let finished = client.wait_for_progress(SYNC_TOKEN, "begin", 1, Duration::from_secs(5));
    assert!(!finished, "dependencySync=off must never start a sync");
    assert!(
        !workspace.gradle_cache().exists(),
        "the build tool must not have run"
    );
}

// ── libraries published without sources: decompiled ─────────────────────────

fn file_text_line(uri: &str, line: u64) -> String {
    let path = tower_lsp::lsp_types::Url::parse(uri)
        .unwrap()
        .to_file_path()
        .unwrap_or_else(|_| panic!("{uri} must be a file:// location an editor can open"));
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .nth(line as usize)
        .unwrap_or_default()
        .to_owned()
}

/// `(line, column)` of `greet` in `greeter.greet(name)` in `Welcome.kt`.
fn greet_call(workspace: &SyncWorkspace) -> (u64, u64) {
    let text = std::fs::read_to_string(workspace.root.join(WELCOME_KT)).unwrap();
    let (line_index, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("greeter.greet(name)"))
        .unwrap();
    (line_index as u64, line.find("greet(").unwrap() as u64)
}

/// Go-to-definition from `greeter.greet(name)` must open the DECOMPILED
/// `Greeter.java` (real code, not a signature stub) at `greet`, and
/// find-references must include the call inside the library's own
/// `greetAll`.
fn assert_decompiled_library_is_navigable(workspace: &SyncWorkspace, client: &mut LspClient) {
    let uri = file_uri(&workspace.root, WELCOME_KT);
    let text = std::fs::read_to_string(workspace.root.join(WELCOME_KT)).unwrap();
    client.open_file(&uri, "kotlin", &text);
    let at = greet_call(workspace);

    let deadline = Instant::now() + LIBRARY_READY_TIMEOUT;
    let definition = loop {
        let found = definition(client, &uri, at);
        if found.iter().any(|(uri, _)| uri.ends_with("/Greeter.java")) || Instant::now() > deadline
        {
            break found;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let (greeter_uri, line) = definition
        .iter()
        .find(|(uri, _)| uri.ends_with("/Greeter.java"))
        .cloned()
        .unwrap_or_else(|| {
            panic!("definition must open the decompiled Greeter.java: {definition:?}")
        });
    let declaration = file_text_line(&greeter_uri, line);
    assert!(
        declaration.contains("public String greet("),
        "must land on the decompiled declaration, got {declaration:?}"
    );

    let response = client.request(
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": at.0, "character": at.1},
            "context": {"includeDeclaration": false},
        }),
    );
    let references = locations(&response["result"]);
    let inside_library: Vec<String> = references
        .iter()
        .filter(|(uri, _)| uri.ends_with("/Greeter.java"))
        .map(|(uri, line)| file_text_line(uri, *line))
        .collect();
    assert!(
        inside_library.iter().any(|line| line.contains("greet(")),
        "references must include greetAll's call inside the decompiled library: {references:?}"
    );
}

#[test]
fn maven_sync_decompiles_a_library_published_without_sources() {
    if !tool_available("mvn") {
        return;
    }
    let repository = tempfile::tempdir().unwrap();
    if !textkit::publish_compiled_greeter(repository.path()) {
        return;
    }
    let repository_url = tower_lsp::lsp_types::Url::from_directory_path(repository.path())
        .unwrap()
        .to_string();
    let pom = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n\
         <modelVersion>4.0.0</modelVersion>\n\
         <groupId>com.acme</groupId><artifactId>app</artifactId><version>1.0.0</version>\n\
         <repositories><repository><id>greeter-fixture</id><url>{repository_url}</url></repository></repositories>\n\
         <dependencies><dependency><groupId>com.acme</groupId><artifactId>greeter</artifactId><version>1.0.0</version></dependency></dependencies>\n\
         </project>\n"
    );
    let workspace = SyncWorkspace::from_fixture("greeter-app", &[("pom.xml", pom)]);
    let mut client = workspace.start(Value::Null);
    assert!(
        client.wait_for_progress(SYNC_TOKEN, "end", 1, SYNC_TIMEOUT),
        "Maven dependency sync never finished"
    );
    assert_decompiled_library_is_navigable(&workspace, &mut client);
}

#[test]
fn gradle_sync_decompiles_a_library_published_without_sources() {
    if !tool_available("gradle") {
        return;
    }
    let repository = tempfile::tempdir().unwrap();
    if !textkit::publish_compiled_greeter(repository.path()) {
        return;
    }
    let repository_url = serve_directory(repository.path().to_path_buf());
    let build = format!(
        "plugins {{ `java-library` }}\n\
         repositories {{\n    maven {{\n        url = uri(\"{repository_url}\")\n        isAllowInsecureProtocol = true\n    }}\n    mavenCentral()\n}}\n\
         dependencies {{ implementation(\"com.acme:greeter:1.0.0\") }}\n"
    );
    let workspace = SyncWorkspace::from_fixture(
        "greeter-app",
        &[
            (
                "settings.gradle.kts",
                "rootProject.name = \"app\"\n".to_owned(),
            ),
            ("build.gradle.kts", build),
        ],
    );
    let mut client = workspace.start(Value::Null);
    assert!(
        client.wait_for_progress(SYNC_TOKEN, "end", 1, SYNC_TIMEOUT),
        "Gradle dependency sync never finished"
    );
    assert_decompiled_library_is_navigable(&workspace, &mut client);
}

#[test]
fn decompile_libraries_off_leaves_libraries_without_sources_alone() {
    if !tool_available("mvn") {
        return;
    }
    let repository = tempfile::tempdir().unwrap();
    if !textkit::publish_compiled_greeter(repository.path()) {
        return;
    }
    let repository_url = tower_lsp::lsp_types::Url::from_directory_path(repository.path())
        .unwrap()
        .to_string();
    let pom = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion>\n\
         <groupId>com.acme</groupId><artifactId>app</artifactId><version>1.0.0</version>\n\
         <repositories><repository><id>greeter-fixture</id><url>{repository_url}</url></repository></repositories>\n\
         <dependencies><dependency><groupId>com.acme</groupId><artifactId>greeter</artifactId><version>1.0.0</version></dependency></dependencies>\n\
         </project>\n"
    );
    let workspace = SyncWorkspace::from_fixture("greeter-app", &[("pom.xml", pom)]);
    let mut client = workspace.start(json!({"indexingOptions": {"decompileLibraries": false}}));
    assert!(
        client.wait_for_progress(SYNC_TOKEN, "end", 1, SYNC_TIMEOUT),
        "Maven dependency sync never finished"
    );
    let decompiled = workspace.base.join("xdg-cache/kmp-lsp/decompiled");
    assert!(
        !decompiled.exists() || std::fs::read_dir(&decompiled).unwrap().next().is_none(),
        "decompileLibraries=false must not decompile anything"
    );
}

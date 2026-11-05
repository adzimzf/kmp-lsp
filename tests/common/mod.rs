//! Shared LSP stdio client and fixture helpers for integration tests.
//!
//! Spawns the compiled `kmp-lsp --stdio` binary and drives it over the
//! JSON-RPC wire protocol. Used by `lsp_smoke.rs` and `java_kotlin_interop.rs`.

#![allow(dead_code)] // each test crate uses a different subset

pub mod textkit;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const BIN: &str = env!("CARGO_BIN_EXE_kmp-lsp");
/// Maximum time to wait for indexing to complete (small synthetic project).
pub const INDEXING_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time to wait for a single LSP response.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// ── minimal LSP client ────────────────────────────────────────────────────────

/// Environment overrides for a spawned server.
#[derive(Default)]
pub struct ServerEnvironment {
    /// `GRADLE_USER_HOME`; an empty per-workspace directory when `None`.
    pub gradle_user_home: Option<std::path::PathBuf>,
    /// Extra variables (`XDG_CACHE_HOME`, `MAVEN_OPTS`, …).
    pub variables: Vec<(String, std::path::PathBuf)>,
}

pub struct LspClient {
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    // Keep the child alive until the client is dropped.
    _child: Child,
    /// Every `$/progress` `(token, kind)` seen so far, in order — waits that
    /// skip unrelated messages must not lose them.
    progress_log: Vec<(String, String)>,
}

impl LspClient {
    /// Spawn the server, set up the reader thread, and return a ready client.
    ///
    /// `workspace_root` is passed as `KMP_LSP_WORKSPACE_ROOT` so the server
    /// uses the synthetic fixture directory even if a real config file exists.
    pub fn spawn(workspace_root: &Path) -> Self {
        Self::spawn_with(workspace_root, &ServerEnvironment::default())
    }

    /// [`LspClient::spawn`] with test-specific environment overrides — see
    /// [`ServerEnvironment`].
    pub fn spawn_with(workspace_root: &Path, environment: &ServerEnvironment) -> Self {
        // Canonicalize the root so the server uses the same path the OS returns
        // when walking files (resolves /var -> /private/var on macOS, short
        // 8.3 names -> long names on Windows).
        let canonical = canonical_root(workspace_root);
        // Point GRADLE_USER_HOME at an empty, test-local directory so the
        // compiled-JAR crawl never scans the machine's real Gradle cache —
        // without this, jar indexing picks up whatever real dependency jars
        // happen to be cached on the box running the test (hundreds to
        // thousands, adding tens of seconds and making results
        // machine-dependent). Tests that need a compiled JAR use
        // `workspace.json`'s `jarPaths` instead, which is unaffected by this.
        let gradle_home = environment
            .gradle_user_home
            .clone()
            .unwrap_or_else(|| canonical.join(".empty-gradle-home-for-tests"));
        std::fs::create_dir_all(&gradle_home).unwrap_or_else(|e| {
            panic!(
                "failed to create isolated GRADLE_USER_HOME at {}: {e} \
                 (silently ignoring this could let jar indexing fall back \
                 to scanning the real machine's Gradle cache, exactly what \
                 this isolation exists to prevent)",
                gradle_home.display()
            )
        });
        let mut command = Command::new(BIN);
        command
            .args(["--stdio"])
            .env("KMP_LSP_WORKSPACE_ROOT", &canonical)
            .env("GRADLE_USER_HOME", &gradle_home)
            // Remove the real Android SDK from view for the same reason
            // GRADLE_USER_HOME is isolated above: a real android.jar on the
            // host turned every parallel smoke-test process into a
            // redundant, CPU-contended JAR-manifest pass, timing these tests
            // out on CI runners that ship a pre-installed SDK.
            .env_remove("ANDROID_HOME")
            .env_remove("ANDROID_SDK_ROOT")
            // Same for the host's JDK sources: parsing a real `src.zip`
            // into every test's empty cache costs seconds per process. A
            // test that wants a JDK passes `KMP_LSP_JDK_SOURCES` itself.
            .env("KMP_LSP_JDK_SOURCES", "off");
        for (name, value) in &environment.variables {
            command.env(name, value);
        }
        let mut child = command
            .current_dir(&canonical)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherit stderr so Rust panics/backtraces reach the test output.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn kmp-lsp");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let (tx, rx) = mpsc::channel::<Value>();

        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                // Read the Content-Length header line.
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => break, // server closed stdout
                    Ok(_) => {}
                }
                let len: usize = match header
                    .trim()
                    .strip_prefix("Content-Length: ")
                    .and_then(|s| s.parse().ok())
                {
                    Some(n) => n,
                    None => continue, // unexpected header; skip
                };

                // Consume the blank separator line (\r\n).
                let mut blank = String::new();
                let _ = reader.read_line(&mut blank);

                // Read the JSON body.
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).is_err() {
                    break;
                }
                let msg: Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });

        LspClient {
            stdin,
            rx,
            next_id: 1,
            progress_log: Vec::new(),
            _child: child,
        }
    }

    /// Write a raw JSON-RPC message to the server's stdin.
    pub fn write_raw(&mut self, msg: &Value) {
        let body = serde_json::to_string(msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    pub fn notify(&mut self, method: &str, params: Value) {
        self.write_raw(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    /// Send a JSON-RPC request and return the matching response.
    ///
    /// Incoming server-originated requests are auto-acknowledged with `null`.
    /// Notifications are silently consumed.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;

        self.write_raw(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let msg = self.receive(remaining).unwrap_or_else(|_| {
                panic!("timeout ({REQUEST_TIMEOUT:?}) waiting for response to `{method}`")
            });

            // Server request (has both `id` and `method`) → auto-ack.
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let server_id = msg["id"].clone();
                self.write_raw(&json!({
                    "jsonrpc": "2.0",
                    "id": server_id,
                    "result": null,
                }));
                continue;
            }

            // Notification (has `method`, no `id`) → skip.
            if msg.get("method").is_some() {
                continue;
            }

            // Response matching our request id → return it.
            if msg.get("id") == Some(&json!(id)) {
                return msg;
            }
        }
    }

    /// Receive the next message, recording it in `progress_log` when it is a
    /// `$/progress` notification.
    fn receive(&mut self, timeout: Duration) -> Result<Value, mpsc::RecvTimeoutError> {
        let message = self.rx.recv_timeout(timeout)?;
        if message.get("method") == Some(&json!("$/progress")) {
            let token = message["params"]["token"].as_str().unwrap_or("").to_owned();
            let kind = message["params"]["value"]["kind"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            self.progress_log.push((token, kind));
        }
        Ok(message)
    }

    /// How many `$/progress` events of `kind` (`begin`/`end`) for `token`
    /// have arrived so far.
    pub fn progress_count(&self, token: &str, kind: &str) -> usize {
        self.progress_log
            .iter()
            .filter(|(seen_token, seen_kind)| seen_token == token && seen_kind == kind)
            .count()
    }

    /// Process incoming messages (acknowledging server requests) until at
    /// least `count` `kind` events for `token` have arrived. Returns `false`
    /// on timeout.
    pub fn wait_for_progress(
        &mut self,
        token: &str,
        kind: &str,
        count: usize,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while self.progress_count(token, kind) < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.receive(remaining) else {
                return false;
            };
            if message.get("method").is_some() && message.get("id").is_some() {
                let server_id = message["id"].clone();
                self.write_raw(&json!({"jsonrpc": "2.0", "id": server_id, "result": null}));
            }
        }
        true
    }

    /// Wait until the server sends a `$/progress` *end* event for the
    /// `kmp-lsp/indexing` token, indicating that workspace indexing is done.
    pub fn wait_for_indexing(&mut self) {
        let deadline = Instant::now() + INDEXING_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let msg = self
                .receive(remaining)
                .expect("timeout waiting for `kmp-lsp/indexing` end — indexing took too long");

            // Auto-ack server requests.
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let server_id = msg["id"].clone();
                self.write_raw(&json!({
                    "jsonrpc": "2.0",
                    "id": server_id,
                    "result": null,
                }));
                continue;
            }

            if msg.get("method") == Some(&json!("$/progress")) {
                let token = msg["params"]["token"].as_str().unwrap_or("");
                let kind = msg["params"]["value"]["kind"].as_str().unwrap_or("");
                if token == "kmp-lsp/indexing" && kind == "end" {
                    return;
                }
            }
        }
    }

    /// Full initialization handshake: send `initialize`, wait for the response,
    /// then send `initialized`.  Does not wait for indexing to finish.
    pub fn initialize(&mut self, root: &Path) {
        self.initialize_with_options(root, Value::Null);
    }

    /// [`LspClient::initialize`] with `initializationOptions`.
    pub fn initialize_with_options(&mut self, root: &Path, initialization_options: Value) {
        let canonical = canonical_root(root);
        let root_uri = tower_lsp::lsp_types::Url::from_file_path(&canonical)
            .expect("valid absolute path")
            .to_string();
        let resp = self.request(
            "initialize",
            json!({
                "rootUri": root_uri,
                "initializationOptions": initialization_options,
                "capabilities": {
                    "textDocument": {
                        "completion": {"completionItem": {"snippetSupport": false}},
                        "definition": {},
                        "inlayHint": {},
                        "hover": {},
                    },
                    "workspace": {"symbol": {}},
                    "window": {"workDoneProgress": true},
                },
            }),
        );
        assert!(
            resp.get("result").is_some(),
            "initialize must succeed; got: {resp}"
        );
        self.notify("initialized", json!({}));
    }

    /// Send `textDocument/didOpen` for a file already on disk.
    pub fn open_file(&mut self, uri: &str, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                },
            }),
        );
    }

    /// Send a whole-document `textDocument/didChange` (single full-text change).
    pub fn change_file(&mut self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"text": text}],
            }),
        );
    }

    /// Wait until a `textDocument/publishDiagnostics` for `uri` carries a
    /// diagnostic whose message contains `needle`, or panic after `timeout`.
    ///
    /// Diagnostics recompute asynchronously (debounced re-index + possible
    /// jar-phase settling), so this keeps consuming publishes for the same
    /// uri rather than asserting on the first one, which may predate the
    /// edit being reflected.
    pub fn wait_for_diagnostic_containing(&mut self, uri: &str, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let msg = self.receive(remaining).unwrap_or_else(|_| {
                panic!(
                    "timeout ({timeout:?}) waiting for a publishDiagnostics on {uri} \
                     containing {needle:?}"
                )
            });
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let server_id = msg["id"].clone();
                self.write_raw(&json!({"jsonrpc": "2.0", "id": server_id, "result": null}));
                continue;
            }
            if msg.get("method") != Some(&json!("textDocument/publishDiagnostics")) {
                continue;
            }
            if msg["params"]["uri"].as_str() != Some(uri) {
                continue;
            }
            let matched = msg["params"]["diagnostics"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|d| {
                    d["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(needle))
                });
            if matched {
                return;
            }
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Best-effort graceful shutdown.
        self.write_raw(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "shutdown",
            "params": null,
        }));
        self.write_raw(&json!({"jsonrpc":"2.0","method":"exit","params":null}));
    }
}

// ── fixture helpers ───────────────────────────────────────────────────────────

/// Returns the canonical form of a path for use as a server workspace root.
///
/// On macOS this resolves /var → /private/var symlinks.
/// On Windows this strips the \\?\ UNC prefix that std::fs::canonicalize adds,
/// so Url::from_file_path and KOTLIN_LSP_WORKSPACE_ROOT env var work correctly.
pub fn canonical_root(path: &Path) -> std::path::PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // Strip the Windows extended-length prefix (\\?\) if present.
    let s = canonical.to_string_lossy();
    if let Some(stripped) = s.strip_prefix("\\\\?\\") {
        std::path::PathBuf::from(stripped)
    } else {
        canonical
    }
}

pub fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    if let Some(p) = full.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

pub fn file_uri(dir: &Path, rel: &str) -> String {
    // Build from the canonical form of the directory so the URI matches what
    // the server returns (server is also spawned with the canonical root).
    let canonical = canonical_root(&dir.join(rel));
    tower_lsp::lsp_types::Url::from_file_path(&canonical)
        .expect("valid absolute file path")
        .to_string()
}

/// Build LSP `Position` (0-based) by counting lines and UTF-16 columns.
pub fn pos(text: &str, line: usize, col: usize) -> Value {
    let _ = text; // kept for documentation; positions are already 0-based
    json!({"line": line, "character": col})
}

// ── result helpers ───────────────────────────────────────────────────────────

/// `(uri, start line)` for every location in a definition/references result
/// (`Location`, `Location[]` or `LocationLink[]`).
pub fn locations(result: &Value) -> Vec<(String, u64)> {
    let items: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };
    items
        .into_iter()
        .filter_map(|item| {
            let uri = item["uri"].as_str().or(item["targetUri"].as_str())?;
            let range = if item["range"].is_object() {
                &item["range"]
            } else {
                &item["targetSelectionRange"]
            };
            Some((uri.to_owned(), range["start"]["line"].as_u64()?))
        })
        .collect()
}

pub fn copy_dir(source: &Path, destination: &Path) {
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

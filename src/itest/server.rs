//! The server a case is asked of, and the LSP conversation with it.
//!
//! By default the server is this crate's own, run inside the test process
//! and reached through an in-memory pipe: no executable is spawned and no
//! fixture file touches the disk — the sources arrive as `didOpen` text.
//!
//! `KMP_ITEST_SERVER` picks another one for the whole run:
//!
//! | value     | server                                          |
//! |-----------|-------------------------------------------------|
//! | `memory`  | in-process kmp-lsp (the default)                |
//! | `kmp-lsp` | the kmp-lsp executable named by `KMP_ITEST_BIN` |
//! | `jdtls`   | jdtls's `bin/jdtls` launcher, in `KMP_ITEST_BIN` |
//!
//! An executable reads its workspace from disk, so for those the fixture is
//! written to a temporary directory first.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::{timeout_at, Instant};
use tower_lsp::lsp_types::Url;
use tower_lsp::{LspService, Server};

use super::fixture::Fixture;
use crate::indexer::Indexer;

const SERVER_VARIABLE: &str = "KMP_ITEST_SERVER";
const BINARY_VARIABLE: &str = "KMP_ITEST_BIN";

/// Where the in-memory server is told its workspace is. Nothing is there.
const MEMORY_ROOT: &str = "/kmp-lsp-itest";
const PIPE_CAPACITY: usize = 1 << 20;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(super) enum Target {
    Memory,
    KmpLspBinary(PathBuf),
    Jdtls(PathBuf),
}

impl Target {
    pub(super) fn from_environment() -> Self {
        let binary = || {
            let path = std::env::var(BINARY_VARIABLE).unwrap_or_else(|_| {
                panic!("{SERVER_VARIABLE} names an executable server: set {BINARY_VARIABLE} to it")
            });
            PathBuf::from(path)
        };
        match std::env::var(SERVER_VARIABLE).as_deref() {
            Err(_) | Ok("memory") => Self::Memory,
            Ok("kmp-lsp") => Self::KmpLspBinary(binary()),
            Ok("jdtls") => Self::Jdtls(binary()),
            Ok(other) => panic!("{SERVER_VARIABLE}={other}: expected memory, kmp-lsp or jdtls"),
        }
    }

    /// How long a case waits for an answer before it takes "nothing" for one.
    fn patience(&self) -> Duration {
        match self {
            Self::Memory => Duration::from_secs(5),
            Self::KmpLspBinary(_) => Duration::from_secs(20),
            Self::Jdtls(_) => Duration::from_secs(120),
        }
    }
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// What keeps a server alive for as long as its session.
enum Host {
    Task(tokio::task::JoinHandle<()>),
    Process {
        _child: tokio::process::Child,
        _workspace: tempfile::TempDir,
    },
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Self::Task(server) = self {
            server.abort();
        }
    }
}

/// An initialized server with every file of a fixture open.
pub(super) struct Session {
    writer: Writer,
    incoming: mpsc::UnboundedReceiver<Value>,
    next_id: u64,
    /// The URIs the server has published diagnostics for.
    diagnosed: HashSet<String>,
    root: Url,
    patience: Duration,
    _host: Host,
}

impl Session {
    pub(super) async fn open(target: &Target, fixture: &Fixture) -> Self {
        let (reader, writer, root, host) = match target {
            Target::Memory => in_memory(),
            Target::KmpLspBinary(binary) => spawned(binary, &[], fixture),
            Target::Jdtls(launcher) => spawned(launcher, &["-data"], fixture),
        };
        let mut session = Self {
            writer,
            incoming: frames(reader),
            next_id: 0,
            diagnosed: HashSet::new(),
            root,
            patience: target.patience(),
            _host: host,
        };
        session.initialize().await;
        if matches!(target, Target::Jdtls(_)) {
            session.wait_for_jdtls().await;
        }
        for file in &fixture.files {
            session.did_open(&file.path, &file.text).await;
        }
        session.wait_until_read(fixture).await;
        session
    }

    pub(super) fn uri(&self, path: &str) -> Url {
        self.root.join(path).expect("a fixture path is relative")
    }

    /// `uri` as the fixture spelled it, or whole when it is outside the
    /// workspace (a JDK or library source).
    pub(super) fn display(&self, uri: &str) -> String {
        let below_root = uri.strip_prefix(self.root.as_str());
        format!("/{}", below_root.unwrap_or(uri))
            .trim_start_matches("//")
            .to_owned()
    }

    /// The `result` of `method`, asked again until `settled` accepts it or
    /// patience runs out: a server answers `null` or a partial list while it
    /// is still reading what was just opened, and a client asks again.
    pub(super) async fn request_until(
        &mut self,
        method: &str,
        params: Value,
        settled: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + self.patience;
        loop {
            let result = self.request(method, params.clone(), deadline).await;
            if settled(&result) || Instant::now() >= deadline {
                return result;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Opening a file only hands it over; the server reads it afterwards,
    /// and reports the file's diagnostics once it has. Waits for those, for
    /// every file, or until patience runs out.
    async fn wait_until_read(&mut self, fixture: &Fixture) {
        let deadline = Instant::now() + self.patience;
        let all_read = |session: &Self| {
            fixture
                .files
                .iter()
                .all(|file| session.diagnosed.contains(session.uri(&file.path).as_str()))
        };
        while !all_read(self) {
            let Some(message) = self.receive(deadline).await else {
                return;
            };
            self.answer_server(&message).await;
        }
    }

    async fn request(&mut self, method: &str, params: Value, deadline: Instant) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let message = self.receive(deadline).await;
            let Some(message) = message else {
                return Value::Null;
            };
            if message.get("method").is_some() {
                self.answer_server(&message).await;
            } else if message["id"] == json!(id) {
                if let Some(error) = message.get("error") {
                    panic!("{method} failed: {error}");
                }
                return message.get("result").cloned().unwrap_or(Value::Null);
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn send(&mut self, message: Value) {
        let body = message.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        self.writer
            .write_all(frame.as_bytes())
            .await
            .expect("the server reads what it is sent");
        self.writer.flush().await.expect("the pipe flushes");
    }

    /// The next message, or `None` once `deadline` passes or the server is gone.
    async fn receive(&mut self, deadline: Instant) -> Option<Value> {
        let message = timeout_at(deadline, self.incoming.recv())
            .await
            .ok()
            .flatten()?;
        if message["method"] == "textDocument/publishDiagnostics" {
            if let Some(uri) = message["params"]["uri"].as_str() {
                self.diagnosed.insert(uri.to_owned());
            }
        }
        Some(message)
    }

    /// Servers ask their client things (`workspace/configuration`,
    /// `client/registerCapability`) and wait: every one gets an empty answer.
    async fn answer_server(&mut self, request: &Value) {
        let Some(id) = request.get("id") else {
            return;
        };
        let sections = request["params"]["items"].as_array().map_or(0, Vec::len);
        let result = match request["method"].as_str() {
            Some("workspace/configuration") => json!(vec![Value::Null; sections]),
            _ => Value::Null,
        };
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    async fn initialize(&mut self) {
        let deadline = Instant::now() + self.patience;
        let params = json!({
            "processId": std::process::id(),
            "rootUri": self.root.as_str().trim_end_matches('/'),
            "capabilities": {
                "textDocument": {
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "rename": {"prepareSupport": true},
                    "signatureHelp": {},
                },
                "workspace": {"workspaceEdit": {"documentChanges": true}},
            },
            "initializationOptions": {
                "settings": {"java": {"signatureHelp": {"enabled": true}}},
            },
        });
        self.request("initialize", params, deadline).await;
        self.notify("initialized", json!({})).await;
    }

    /// jdtls answers nothing useful before it reports `ServiceReady`.
    async fn wait_for_jdtls(&mut self) {
        let deadline = Instant::now() + self.patience;
        while let Some(message) = self.receive(deadline).await {
            if message["method"] == "language/status" && message["params"]["type"] == "ServiceReady"
            {
                return;
            }
            self.answer_server(&message).await;
        }
        panic!("jdtls never reported ServiceReady");
    }

    async fn did_open(&mut self, path: &str, text: &str) {
        let language = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map_or("java", |extension| match extension {
                "kt" | "kts" => "kotlin",
                "swift" => "swift",
                _ => "java",
            });
        let document = json!({
            "uri": self.uri(path),
            "languageId": language,
            "version": 1,
            "text": text,
        });
        self.notify("textDocument/didOpen", json!({"textDocument": document}))
            .await;
    }
}

/// This crate's server on the other end of an in-memory pipe.
fn in_memory() -> (Reader, Writer, Url, Host) {
    let (client_side, server_side) = tokio::io::duplex(PIPE_CAPACITY);
    let (server_reader, server_writer) = tokio::io::split(server_side);
    let (service, socket) =
        LspService::new(|client| crate::backend_over(Arc::new(rootless_indexer()), client));
    let server = tokio::spawn(Server::new(server_reader, server_writer, socket).serve(service));
    let (reader, writer) = tokio::io::split(client_side);
    let root = Url::parse(&format!("file://{MEMORY_ROOT}/")).expect("a file URI");
    (Box::new(reader), Box::new(writer), root, Host::Task(server))
}

/// An indexer that never looks for a workspace: with a root pinned to
/// nowhere, opening a file indexes its text and nothing else — no directory
/// scan, no Gradle cache, no JDK sources.
fn rootless_indexer() -> Indexer {
    let indexer = Indexer::new();
    indexer.workspace_pinned.store(true, Ordering::Relaxed);
    indexer
}

/// `binary` started on a temporary directory holding the fixture's files.
/// `data_flag` is how the server is told where to keep its own state.
fn spawned(binary: &Path, data_flag: &[&str], fixture: &Fixture) -> (Reader, Writer, Url, Host) {
    let workspace = tempfile::tempdir().expect("a temporary directory");
    let project = workspace.path().join("project");
    for file in &fixture.files {
        let destination = project.join(&file.path);
        std::fs::create_dir_all(destination.parent().expect("a file has a directory"))
            .expect("the fixture's directories are created");
        std::fs::write(&destination, &file.text).expect("the fixture's files are written");
    }
    let mut command = tokio::process::Command::new(binary);
    command
        .args(data_flag)
        .args(data_flag.iter().map(|_| workspace.path().join("state")))
        .env("KMP_LSP_WORKSPACE_ROOT", &project)
        .env("XDG_CACHE_HOME", workspace.path().join("cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("{} did not start: {error}", binary.display()));
    let reader = child.stdout.take().expect("stdout is piped");
    let writer = child.stdin.take().expect("stdin is piped");
    let root = Url::from_directory_path(&project).expect("an absolute path");
    let host = Host::Process {
        _child: child,
        _workspace: workspace,
    };
    (Box::new(reader), Box::new(writer), root, host)
}

/// The `Content-Length` framed messages `reader` carries, one per item.
fn frames(reader: Reader) -> mpsc::UnboundedReceiver<Value> {
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        while let Some(message) = read_frame(&mut reader).await {
            if sender.send(message).is_err() {
                return;
            }
        }
    });
    receiver
}

async fn read_frame(reader: &mut BufReader<Reader>) -> Option<Value> {
    let mut length = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.ok()? == 0 {
            return None;
        }
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

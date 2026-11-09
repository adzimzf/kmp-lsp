//! The `vscode.java.*` delegate commands of java-debug's jdtls plugin
//! (`com.microsoft.java.debug.plugin/plugin.xml`), answered by kmp-lsp.
//!
//! Commands whose jdtls answer needs the Eclipse build (hot code replace,
//! `buildWorkspace`, `resolveElementAtSelection`, inline values) are not
//! offered; editors fall back without them.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use super::launch::{self, ClasspathScope};
use super::server::DebugServer;
use crate::indexer::Indexer;

/// Every command [`DebugCommands::execute`] answers.
pub(crate) const COMMANDS: &[&str] = &[
    START_DEBUG_SESSION,
    RESOLVE_MAIN_CLASS,
    RESOLVE_CLASSPATH,
    RESOLVE_JAVA_EXECUTABLE,
    UPDATE_DEBUG_SETTINGS,
    VALIDATE_LAUNCH_CONFIG,
    CHECK_PROJECT_SETTINGS,
    IS_ON_CLASSPATH,
    FETCH_PLATFORM_SETTINGS,
    INFER_LAUNCH_COMMAND_LENGTH,
    RESOLVE_SOURCE_URI,
];

const START_DEBUG_SESSION: &str = "vscode.java.startDebugSession";
const RESOLVE_MAIN_CLASS: &str = "vscode.java.resolveMainClass";
const RESOLVE_CLASSPATH: &str = "vscode.java.resolveClasspath";
const RESOLVE_JAVA_EXECUTABLE: &str = "vscode.java.resolveJavaExecutable";
const UPDATE_DEBUG_SETTINGS: &str = "vscode.java.updateDebugSettings";
const VALIDATE_LAUNCH_CONFIG: &str = "vscode.java.validateLaunchConfig";
const CHECK_PROJECT_SETTINGS: &str = "vscode.java.checkProjectSettings";
const IS_ON_CLASSPATH: &str = "vscode.java.isOnClasspath";
const FETCH_PLATFORM_SETTINGS: &str = "vscode.java.fetchPlatformSettings";
const INFER_LAUNCH_COMMAND_LENGTH: &str = "vscode.java.inferLaunchCommandLength";
const RESOLVE_SOURCE_URI: &str = "vscode.java.resolveSourceUri";

/// The newest Java release the debug server's JDI handles, as jdtls'
/// `fetchPlatformSettings` reports it.
const LATEST_SUPPORTED_JAVA_VERSION: &str = "21";

/// What a command answered.
pub(crate) enum CommandOutcome {
    /// Not a debug command; let the caller try its own.
    NotDebugCommand,
    Answered(Value),
    Failed(String),
}

/// The debug half of kmp-lsp's `workspace/executeCommand`.
pub(crate) struct DebugCommands {
    indexer: Arc<Indexer>,
    server: tokio::sync::Mutex<Option<DebugServer>>,
    /// The last `updateDebugSettings` JSON, replayed into a restarted server.
    settings: std::sync::Mutex<Option<String>>,
}

impl DebugCommands {
    pub(crate) fn new(indexer: Arc<Indexer>) -> Self {
        Self {
            indexer,
            server: tokio::sync::Mutex::new(None),
            settings: std::sync::Mutex::new(None),
        }
    }

    pub(crate) async fn execute(&self, command: &str, arguments: &[Value]) -> CommandOutcome {
        match command {
            START_DEBUG_SESSION => match self.start_debug_session().await {
                Ok(port) => CommandOutcome::Answered(Value::from(port)),
                Err(reason) => CommandOutcome::Failed(reason),
            },
            RESOLVE_MAIN_CLASS => {
                let under = string_argument(arguments, 0).and_then(path_of_argument);
                let indexer = Arc::clone(&self.indexer);
                let found =
                    blocking(move || launch::resolve_main_classes(&indexer, under.as_deref()))
                        .await;
                CommandOutcome::Answered(serde_json::to_value(found).unwrap_or(Value::Null))
            }
            RESOLVE_CLASSPATH => {
                let Some(main_class) = string_argument(arguments, 0) else {
                    return CommandOutcome::Failed("resolveClasspath needs a main class".into());
                };
                let main_class = strip_module_prefix(main_class).to_owned();
                let project_name = string_argument(arguments, 1).map(str::to_owned);
                let scope = ClasspathScope::parse(string_argument(arguments, 2));
                let indexer = Arc::clone(&self.indexer);
                let classpath = blocking(move || {
                    launch::resolve_classpath(&indexer, &main_class, project_name.as_deref(), scope)
                })
                .await;
                // jdtls answers `[modulepaths, classpaths]`; kmp-lsp puts
                // everything on the classpath.
                CommandOutcome::Answered(serde_json::json!([[], classpath]))
            }
            RESOLVE_JAVA_EXECUTABLE => {
                let java = launch::java_executable(&self.indexer);
                CommandOutcome::Answered(
                    java.map(|path| Value::from(path.to_string_lossy().into_owned()))
                        .unwrap_or(Value::Null),
                )
            }
            UPDATE_DEBUG_SETTINGS => {
                let settings = string_argument(arguments, 0)
                    .map(str::to_owned)
                    .or_else(|| arguments.first().map(Value::to_string));
                if let Some(settings) = settings {
                    self.update_settings(settings).await;
                }
                CommandOutcome::Answered(Value::Null)
            }
            VALIDATE_LAUNCH_CONFIG => CommandOutcome::Answered(serde_json::json!({
                "mainClass": { "isValid": true },
                "projectName": { "isValid": true },
                "proposals": [],
            })),
            CHECK_PROJECT_SETTINGS | IS_ON_CLASSPATH => CommandOutcome::Answered(Value::Bool(true)),
            FETCH_PLATFORM_SETTINGS => CommandOutcome::Answered(serde_json::json!({
                "latestSupportedJavaVersion": LATEST_SUPPORTED_JAVA_VERSION,
            })),
            // The command line kmp-lsp's launcher builds is never shortened.
            INFER_LAUNCH_COMMAND_LENGTH => CommandOutcome::Answered(Value::from(0)),
            RESOLVE_SOURCE_URI => {
                let Some(class_name) = string_argument(arguments, 0) else {
                    return CommandOutcome::Answered(Value::Null);
                };
                let class_name = class_name.to_owned();
                let indexer = Arc::clone(&self.indexer);
                let uri =
                    blocking(move || super::server::source_uri(&indexer, &class_name, None)).await;
                CommandOutcome::Answered(uri.map(Value::from).unwrap_or(Value::Null))
            }
            _ => CommandOutcome::NotDebugCommand,
        }
    }

    /// The port of the (possibly just started) debug server.
    async fn start_debug_session(&self) -> Result<u16, String> {
        let mut server = self.server.lock().await;
        if let Some(running) = server.as_mut() {
            if running.is_alive() {
                return Ok(running.port());
            }
            log::warn!("debug: kmp-debug-server exited — restarting it");
        }
        let java = launch::java_executable(&self.indexer)
            .ok_or_else(|| super::server::LaunchError::JavaNotFound.to_string())?;
        let settings = self
            .settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let indexer = Arc::clone(&self.indexer);
        let started = blocking(move || DebugServer::launch(indexer, &java, settings.as_deref()))
            .await
            .map_err(|error| error.to_string())?;
        let port = started.port();
        *server = Some(started);
        Ok(port)
    }

    async fn update_settings(&self, settings: String) {
        if let Some(running) = self.server.lock().await.as_ref() {
            running.update_settings(&settings);
        }
        *self
            .settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(settings);
    }
}

fn string_argument(arguments: &[Value], index: usize) -> Option<&str> {
    arguments
        .get(index)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// `resolveMainClass`' argument is a file/folder URI (or path) to search
/// under; a project name, which jdtls also accepts, searches everything.
fn path_of_argument(argument: &str) -> Option<PathBuf> {
    if argument.starts_with("file:") {
        return launch::file_path_of(argument);
    }
    let path = PathBuf::from(argument);
    path.is_absolute().then_some(path)
}

/// jdtls prefixes a main class with its Java module (`my.module/com.Main`).
fn strip_module_prefix(main_class: &str) -> &str {
    main_class
        .rsplit_once('/')
        .map_or(main_class, |(_, class_name)| class_name)
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(join_error) => std::panic::resume_unwind(join_error.into_panic()),
    }
}

#[cfg(test)]
#[path = "commands_tests.rs"]
mod tests;

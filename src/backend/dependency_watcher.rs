//! Background dependency sync (see [`crate::dependency_sync`]).
//!
//! On startup, and whenever the build files change, the project's build tool
//! resolves and downloads its dependencies (sources included); when that
//! changed anything, the workspace is reindexed so the JAR pipeline mounts
//! them. A change is noticed three ways: an editor's save of a build file, a
//! `workspace/didChangeWatchedFiles` for one (both call
//! [`DependencySyncTrigger::wake`]), and — for edits made outside the
//! editor — a slow poll of the build files' fingerprint.
//!
//! A failed sync is reported once and not retried until the build files
//! change again (or a sync is requested with `kmp-lsp/syncDependencies`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Notify};
use tower_lsp::lsp_types::{MessageType, NumberOrString};
use tower_lsp::Client;

use crate::dependency_sync::{SyncMode, SyncOutcome};
use crate::indexer::ProgressReporter;
use crate::workspace::Event;

use super::progress::LspProgressReporter;

/// The `$/progress` token for a running sync.
pub(crate) const SYNC_PROGRESS_TOKEN: &str = "kmp-lsp/dependency-sync";

/// Fallback check for build-file edits the editor didn't report.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Wakes the watcher early: a build file was saved or changed, or a sync was
/// requested.
#[derive(Default)]
pub(crate) struct DependencySyncTrigger {
    wake: Notify,
    force: AtomicBool,
}

impl DependencySyncTrigger {
    /// Re-check the build files now.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Run the build tool now even if the build files are unchanged.
    pub(crate) fn force(&self) {
        self.force.store(true, Ordering::Release);
        self.wake.notify_one();
    }
}

/// Start the watcher for `root` unless `mode` is [`SyncMode::Off`].
pub(super) fn spawn_dependency_sync(
    root: PathBuf,
    mode: SyncMode,
    trigger: Arc<DependencySyncTrigger>,
    event_tx: mpsc::Sender<Event>,
    client: Client,
) {
    if mode == SyncMode::Off {
        log::info!("dependency sync: off");
        return;
    }
    tokio::spawn(async move {
        // The fingerprint last synced or attempted — a failure is not
        // retried until the build files change.
        let mut handled_fingerprint: Option<String> = None;
        // Failed sources downloads are retried no more often than this.
        let mut next_sources_retry = tokio::time::Instant::now();
        loop {
            let force = trigger.force.swap(false, Ordering::AcqRel);
            let (fingerprint, sources_retry_due) = {
                let root = root.clone();
                tokio::task::spawn_blocking(move || {
                    let fingerprint = crate::dependency_sync::build_fingerprint(&root);
                    let retry_due = fingerprint.as_deref().is_some_and(|fingerprint| {
                        crate::dependency_sync::sources_retry_due(&root, fingerprint)
                    });
                    (fingerprint, retry_due)
                })
                .await
                .unwrap_or((None, false))
            };
            let changed = fingerprint.is_some() && fingerprint != handled_fingerprint;
            let retry_now = sources_retry_due && tokio::time::Instant::now() >= next_sources_retry;
            if force || changed || retry_now {
                handled_fingerprint = fingerprint;
                next_sources_retry =
                    tokio::time::Instant::now() + crate::dependency_sync::RETRY_BACK_OFF;
                let keep_running = sync_once(&root, force, &event_tx, &client).await;
                if !keep_running {
                    return;
                }
            }
            tokio::select! {
                _ = trigger.wake.notified() => {}
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    });
}

/// One sync (skipped inside `sync` when the saved manifest is current).
/// Returns `false` when the workspace actor is gone and the watcher should
/// stop.
async fn sync_once(
    root: &std::path::Path,
    force: bool,
    event_tx: &mpsc::Sender<Event>,
    client: &Client,
) -> bool {
    let up_to_date = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::dependency_sync::build_fingerprint(&root).is_some_and(|fingerprint| {
                crate::dependency_sync::manifest_matches(&root, &fingerprint)
            })
        })
        .await
        .unwrap_or(false)
    };
    if up_to_date && !force {
        return true;
    }

    let reporter = LspProgressReporter(client.clone());
    let token = NumberOrString::String(SYNC_PROGRESS_TOKEN.to_owned());
    reporter
        .begin(&token, "Resolving dependencies and downloading sources…")
        .await;
    let outcome = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || crate::dependency_sync::sync(&root, force))
            .await
            .unwrap_or_else(|error| SyncOutcome::Failed(format!("sync task panicked: {error}")))
    };
    let summary = match &outcome {
        SyncOutcome::Synced {
            dependencies,
            sources,
        } => format!("{dependencies} dependencies, {sources} with sources"),
        SyncOutcome::UpToDate => "dependencies up to date".to_owned(),
        SyncOutcome::NoBuildTool => "no Gradle or Maven build".to_owned(),
        SyncOutcome::Failed(_) => "dependency sync failed".to_owned(),
    };
    reporter.end(&token, &summary).await;

    match outcome {
        SyncOutcome::Synced { .. } => {
            log::info!("dependency sync: {summary} — reindexing");
            if event_tx.send(Event::Reindex).await.is_err() {
                log::warn!("dependency sync: workspace actor is gone, stopping");
                return false;
            }
        }
        SyncOutcome::Failed(message) => {
            log::warn!("dependency sync failed: {message}");
            client
                .show_message(
                    MessageType::WARNING,
                    format!(
                        "kmp-lsp: couldn't resolve dependencies — library sources may be \
                         missing. {}",
                        headline(&message)
                    ),
                )
                .await;
        }
        SyncOutcome::UpToDate | SyncOutcome::NoBuildTool => {}
    }
    true
}

/// The popup's text: what failed and, when the tool said why, its first
/// reason — the full output stays in the log.
fn headline(message: &str) -> String {
    let mut lines = message.lines().filter(|line| !line.trim().is_empty());
    let what = lines.next().unwrap_or(message).trim_end_matches(':');
    match lines.next() {
        Some(reason) => {
            let reason: String = reason.chars().take(300).collect();
            format!("{what}: {reason} (details in the kmp-lsp log)")
        }
        None => what.to_owned(),
    }
}

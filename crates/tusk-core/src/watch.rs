//! Debounced vault watcher feeding `Indexer::refresh_path`.

use crate::error::CoreError;
use crate::index::Indexer;
use crate::vault::VaultStore;
use notify::{RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// Re-index every pending path, expanding directories into the records
/// beneath them, then clear the set.
fn drain(pending: &mut HashSet<PathBuf>, vault: &VaultStore, indexer: &Indexer) {
    for path in pending.drain() {
        for record in records_under(path) {
            let _ = indexer.refresh_path(vault, &record);
        }
    }
}

/// The record files a changed path stands for: a file stands for itself, a
/// directory for every record beneath it.
///
/// A newly created scope directory can arrive as a single event with its
/// records already written: watchers that subscribe per-directory (inotify)
/// only register the new directory *after* its create event, so writes that
/// land in between are never reported on their own. Rescanning the directory
/// recovers them. Watchers that report whole subtrees (FSEvents) deliver the
/// file events too, and `refresh_path` is idempotent, so this costs only a
/// `read_dir` there.
fn records_under(path: PathBuf) -> Vec<PathBuf> {
    if !path.is_dir() {
        return vec![path];
    }
    let mut out = Vec::new();
    let mut stack = vec![path];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if name.ends_with(".md") {
                out.push(path);
            }
        }
    }
    out
}

/// Watches `vault/memory/**` and re-indexes changed paths after a debounce
/// window (≥50ms). Partial writes are tolerated: parse failures are skipped
/// and retried on the next event (build-loop §3.4).
pub struct VaultWatcher {
    watcher: Option<notify::RecommendedWatcher>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl VaultWatcher {
    pub fn start(
        vault: Arc<VaultStore>,
        indexer: Arc<Indexer>,
        debounce: Duration,
    ) -> Result<VaultWatcher, CoreError> {
        let debounce = debounce.max(Duration::from_millis(50));
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event.paths);
                }
            })
            .map_err(|e| CoreError::Other(format!("watcher init: {e}")))?;
        let memory_dir = vault.memory_dir();
        watcher
            .watch(&memory_dir, RecursiveMode::Recursive)
            .map_err(|e| CoreError::Other(format!("watch {}: {e}", memory_dir.display())))?;

        let handle = std::thread::spawn(move || {
            let mut pending: HashSet<PathBuf> = HashSet::new();
            loop {
                let timeout = if pending.is_empty() {
                    Duration::from_secs(3600)
                } else {
                    debounce
                };
                match rx.recv_timeout(timeout) {
                    Ok(paths) => {
                        pending.extend(paths);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        drain(&mut pending, &vault, &indexer);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        drain(&mut pending, &vault, &indexer);
                        break;
                    }
                }
            }
        });

        Ok(VaultWatcher {
            watcher: Some(watcher),
            handle: Some(handle),
        })
    }

    /// Stop watching and join the worker thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // Dropping the notify watcher drops the event sender, which
        // disconnects the channel and ends the worker thread.
        self.watcher.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VaultWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

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
                        for path in pending.drain() {
                            let _ = indexer.refresh_path(&vault, &path);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        for path in pending.drain() {
                            let _ = indexer.refresh_path(&vault, &path);
                        }
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

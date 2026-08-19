//! Core ownership: advisory lock + TuskContext + rebuild (+ watcher).
//! Exactly one CoreHost exists per vault at a time (spec §2.1).

use crate::config::Config;
use crate::platform::VaultLock;
use std::sync::Arc;
use tusk_core::clock::SystemClock;
use tusk_core::error::CoreError;
use tusk_core::watch::VaultWatcher;
use tusk_mcp::TuskContext;

pub struct CoreHost {
    pub ctx: Arc<TuskContext>,
    watcher: Option<VaultWatcher>,
    _lock: VaultLock,
}

impl CoreHost {
    /// Acquire the vault lock, open the core, bring up sync state when
    /// enabled (D21: identities + journal + reconciliation scan), rebuild
    /// the index (D10), and optionally start the watcher.
    pub fn open(config: &Config, with_watcher: bool) -> Result<CoreHost, CoreError> {
        let lock = VaultLock::acquire(&config.vault)?;
        Self::open_locked(config, with_watcher, lock)
    }

    /// Like [`open`](Self::open), but if an embedded `tuskd mcp` session
    /// holds the vault, ask it to yield and wait up to `wait` (D40). For
    /// the daemon and one-shot admin commands — never for another embedded
    /// session, which would just trade the lock back and forth.
    pub fn open_or_request(
        config: &Config,
        with_watcher: bool,
        wait: std::time::Duration,
    ) -> Result<CoreHost, CoreError> {
        let lock = VaultLock::acquire_or_request(&config.vault, wait)?;
        Self::open_locked(config, with_watcher, lock)
    }

    fn open_locked(
        config: &Config,
        with_watcher: bool,
        lock: VaultLock,
    ) -> Result<CoreHost, CoreError> {
        let ctx = Arc::new(TuskContext::open_with(
            &config.vault,
            Arc::new(SystemClock),
            config.policies.clone(),
            config.ranking,
            config.graduation,
        )?);
        if config.sync_enabled {
            crate::sync::enable(&ctx.vault)?;
        }
        ctx.indexer.rebuild(&ctx.vault)?;
        let watcher = if with_watcher {
            Some(VaultWatcher::start(
                Arc::clone(&ctx.vault),
                Arc::clone(&ctx.indexer),
                std::time::Duration::from_millis(100),
            )?)
        } else {
            None
        };
        Ok(CoreHost {
            ctx,
            watcher,
            _lock: lock,
        })
    }

    /// Stop the watcher (tied to transport close — build-loop §3.1) and
    /// release the lock.
    pub fn shutdown(mut self) {
        if let Some(w) = self.watcher.take() {
            w.stop();
        }
        // _lock drops here, releasing flock.
    }
}

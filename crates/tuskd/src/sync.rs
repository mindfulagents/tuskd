//! Sync groundwork wiring (M0, D20): identities + journal, enabled only when
//! `[sync] enabled = true` in tuskd.toml. Local-only — no networking here.

use std::sync::Arc;
use tusk_core::error::CoreError;
use tusk_core::sync::{self, Journal};
use tusk_core::vault::VaultStore;

/// What `enable` established (callers may surface it in status output later).
pub struct SyncBootstrap {
    pub vault_id: String,
    pub device_public_key_pem: String,
    pub reconciled_ops: usize,
}

/// Bring up sync state for a vault at core open:
/// 1. `.tusk/sync/` (0700 — it holds the device private key),
/// 2. vault_id (ULID, created on first open, stable after),
/// 3. device ed25519 key at `device.pem` (lazily generated, 0600 via
///    `platform::write_private`, D17 custody pattern; validated on reopen),
/// 4. the hash-chained journal, attached to the VaultStore,
/// 5. a reconciliation scan for edits made while no daemon was running (D10).
pub fn enable(vault: &Arc<VaultStore>) -> Result<SyncBootstrap, CoreError> {
    let sync_dir = vault.tusk_dir().join("sync");
    crate::platform::create_private_dir(&sync_dir)?;

    let vault_id = sync::load_or_create_vault_id(&sync_dir, vault.clock())?;

    let key_path = sync_dir.join(sync::DEVICE_KEY_FILE);
    let device_public_key_pem = if key_path.exists() {
        let pem = std::fs::read_to_string(&key_path)
            .map_err(|e| CoreError::io(key_path.display().to_string(), e))?;
        sync::device_public_key_pem(&pem)?
    } else {
        let (private_pem, public_pem) = sync::generate_device_key()?;
        crate::platform::write_private(&key_path, &private_pem)?;
        public_pem
    };

    let journal = Journal::open(
        &sync_dir.join(sync::JOURNAL_FILE),
        &vault_id,
        Arc::clone(vault.clock()),
    )?;
    vault.attach_journal(Arc::new(journal));
    let reconciled_ops = sync::reconcile(vault)?;

    Ok(SyncBootstrap {
        vault_id,
        device_public_key_pem,
        reconciled_ops,
    })
}

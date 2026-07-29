//! Sync groundwork (M0, HOT_CACHE_SYNC_PROPOSAL §6 items 1–2; DECISIONS D20):
//! vault/device identity and the append-only, hash-chained change journal.
//! Local-only — no networking, no crypto beyond identity keys.

use crate::clock::Clock;
use crate::error::CoreError;
use crate::vault::{trunc_ms, VaultStore};
use chrono::SecondsFormat;
use ed25519_dalek::pkcs8::EncodePublicKey;
use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// File names under `.tusk/sync/`.
pub const VAULT_ID_FILE: &str = "vault_id";
pub const DEVICE_KEY_FILE: &str = "device.pem";
pub const JOURNAL_FILE: &str = "journal";

/// sha256 hex of raw bytes — the journal's `content_hash` (whole file bytes,
/// not the trimmed-body dedup hash of `record::body_hash`).
pub fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Vault identity
// ---------------------------------------------------------------------------

/// Read `sync_dir/vault_id`, or generate a ULID (from the clock) on first
/// open and persist it atomically (tmp + rename, like all vault writes).
/// Stable across restarts.
pub fn load_or_create_vault_id(
    sync_dir: &Path,
    clock: &Arc<dyn Clock>,
) -> Result<String, CoreError> {
    let path = sync_dir.join(VAULT_ID_FILE);
    if path.exists() {
        let raw =
            fs::read_to_string(&path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
        let id = raw.trim().to_string();
        ulid::Ulid::from_string(&id)
            .map_err(|e| CoreError::Journal(format!("bad vault_id {id:?}: {e}")))?;
        return Ok(id);
    }
    fs::create_dir_all(sync_dir).map_err(|e| CoreError::io(sync_dir.display().to_string(), e))?;
    let now = trunc_ms(clock.now());
    let id = ulid::Ulid::from_datetime(now.into()).to_string();
    let tmp = sync_dir.join(format!(".{VAULT_ID_FILE}.tmp"));
    fs::write(&tmp, format!("{id}\n")).map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
    fs::rename(&tmp, &path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Device identity (keygen/parse only — persistence with 0600 perms lives in
// tuskd via platform::write_private, mirroring the D17 agent-key custody
// split: tusk-core mints key material, tuskd owns file permissions.)
// ---------------------------------------------------------------------------

fn pkcs8_err(what: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Journal(format!("device key {what}: {e}"))
}

/// Generate a fresh ed25519 device keypair; returns `(private_pem, public_pem)`
/// in PKCS#8 / SPKI PEM, same encoding as agent keys (keyring.rs).
pub fn generate_device_key() -> Result<(String, String), CoreError> {
    let signing = SigningKey::generate(&mut rand::rngs::OsRng);
    let private_pem = signing
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| pkcs8_err("encode", e))?
        .to_string();
    let public_pem = signing
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| pkcs8_err("spki encode", e))?;
    Ok((private_pem, public_pem))
}

/// Parse a stored device private key PEM and return its public key PEM
/// (validates the stored key on every open).
pub fn device_public_key_pem(private_pem: &str) -> Result<String, CoreError> {
    let signing = SigningKey::from_pkcs8_pem(private_pem).map_err(|e| pkcs8_err("decode", e))?;
    signing
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| pkcs8_err("spki encode", e))
}

// ---------------------------------------------------------------------------
// Change journal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    /// Full object state: new record, full rewrite, or offline edit.
    Put,
    /// Metadata mutation through a live VaultStore (invalidate / telemetry).
    Patch,
    /// Deletion — forget (or offline removal) leaves a trace.
    Tombstone,
}

/// One journal line. `prev` is the sha256 hex of the previous line's bytes
/// (genesis entries chain from a hash bound to the vault_id), which makes the
/// file an append-only hash chain: any in-place edit breaks verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub op_id: String,
    pub kind: OpKind,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub ts: String,
    pub prev: String,
}

struct JournalState {
    last_hash: String,
}

/// Append-only, hash-chained JSONL journal at `.tusk/sync/journal`.
///
/// Crash-safety matches the vault's atomic-write posture (build-loop §3.4):
/// each append is a single `write_all` of one full line; a crash mid-append
/// leaves only a torn *final* line, which `open` self-heals by truncation
/// (the lost op is re-derived by the reconciliation scan). Any corruption
/// before the final line fails verification hard.
pub struct Journal {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    state: Mutex<JournalState>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal").field("path", &self.path).finish()
    }
}

/// Chain seed: binds a journal to its vault identity, so a journal copied
/// between vaults with different ids fails verification from line one.
pub fn genesis_hash(vault_id: &str) -> String {
    hex::encode(Sha256::digest(
        format!("tuskd-journal:{vault_id}").as_bytes(),
    ))
}

impl Journal {
    /// Open (creating if absent) and verify the full hash chain. A torn
    /// final line (no trailing newline — an interrupted append) is truncated
    /// away; anything else that fails to parse or chain is an error.
    pub fn open(path: &Path, vault_id: &str, clock: Arc<dyn Clock>) -> Result<Journal, CoreError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        }
        let raw = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(CoreError::io(path.display().to_string(), e)),
        };

        let mut running = genesis_hash(vault_id);
        let mut lineno = 0usize;
        let mut good_end = 0usize;
        let mut start = 0usize;
        while let Some(nl) = raw[start..].iter().position(|b| *b == b'\n') {
            let line = &raw[start..start + nl];
            lineno += 1;
            let entry: JournalEntry = serde_json::from_slice(line).map_err(|e| {
                CoreError::Journal(format!(
                    "{}: line {lineno} unparseable: {e}",
                    path.display()
                ))
            })?;
            if entry.prev != running {
                return Err(CoreError::Journal(format!(
                    "{}: hash chain broken at line {lineno} (expected prev {running}, found {})",
                    path.display(),
                    entry.prev
                )));
            }
            running = content_hash(line);
            start += nl + 1;
            good_end = start;
        }
        if good_end < raw.len() {
            // Torn final append: drop the partial line (self-heal, D10 spirit;
            // reconciliation re-derives the op from the files themselves).
            let file = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| CoreError::io(path.display().to_string(), e))?;
            file.set_len(good_end as u64)
                .map_err(|e| CoreError::io(path.display().to_string(), e))?;
        }

        Ok(Journal {
            path: path.to_path_buf(),
            clock,
            state: Mutex::new(JournalState { last_hash: running }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry; `op_id` is a ULID from the clock, `ts` the clock's
    /// now (ms precision). Returns the entry as written.
    pub fn append(
        &self,
        kind: OpKind,
        rel_path: &str,
        content_hash_hex: Option<&str>,
    ) -> Result<JournalEntry, CoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CoreError::Journal("journal state poisoned".into()))?;
        let now = trunc_ms(self.clock.now());
        let entry = JournalEntry {
            op_id: ulid::Ulid::from_datetime(now.into()).to_string(),
            kind,
            path: rel_path.to_string(),
            content_hash: content_hash_hex.map(|h| h.to_string()),
            ts: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            prev: state.last_hash.clone(),
        };
        let line = serde_json::to_string(&entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        file.write_all(format!("{line}\n").as_bytes())
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        state.last_hash = content_hash(line.as_bytes());
        Ok(entry)
    }

    /// All verified entries (the chain was checked at `open`; this re-parses
    /// complete lines only).
    pub fn entries(&self) -> Result<Vec<JournalEntry>, CoreError> {
        let raw = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(CoreError::io(self.path.display().to_string(), e)),
        };
        let mut out = Vec::new();
        for line in raw.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            out.push(serde_json::from_slice(line)?);
        }
        Ok(out)
    }

    /// Fold the journal into last-known live state: `rel_path -> content_hash`
    /// for every path whose latest op is not a tombstone.
    pub fn live_state(&self) -> Result<BTreeMap<String, String>, CoreError> {
        let mut live = BTreeMap::new();
        for entry in self.entries()? {
            match entry.kind {
                OpKind::Tombstone => {
                    live.remove(&entry.path);
                }
                OpKind::Put | OpKind::Patch => {
                    if let Some(h) = entry.content_hash {
                        live.insert(entry.path, h);
                    }
                }
            }
        }
        Ok(live)
    }
}

// ---------------------------------------------------------------------------
// Reconciliation scan (D10 pattern, applied to the journal)
// ---------------------------------------------------------------------------

/// Detect offline edits on vault open: diff `memory/**` on disk against the
/// journal's live state and append `put` entries for new/changed files and
/// `tombstone` entries for files that vanished while no journal was running.
/// No-op (Ok(0)) when the vault has no journal attached. Idempotent.
pub fn reconcile(vault: &VaultStore) -> Result<usize, CoreError> {
    let Some(journal) = vault.journal() else {
        return Ok(0);
    };
    let mut live = journal.live_state()?;
    let mut on_disk: Vec<(String, String)> = Vec::new();
    let mut stack = vec![vault.memory_dir()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if !name.starts_with('.') {
                    stack.push(path);
                }
            } else if name.ends_with(".md") && !name.starts_with('.') {
                let bytes =
                    fs::read(&path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
                on_disk.push((vault.rel_path(&path)?, content_hash(&bytes)));
            }
        }
    }
    on_disk.sort();

    let mut appended = 0usize;
    for (rel, hash) in on_disk {
        match live.remove(&rel) {
            Some(known) if known == hash => {}
            _ => {
                journal.append(OpKind::Put, &rel, Some(&hash))?;
                appended += 1;
            }
        }
    }
    for rel in live.into_keys() {
        journal.append(OpKind::Tombstone, &rel, None)?;
        appended += 1;
    }
    Ok(appended)
}

//! D28: the daemon auto-sync worker — incremental, oplog-driven cloud sync
//! that replaces manual `tuskd sync push|pull` on connected vaults.
//!
//! Each cycle is pull-then-push:
//!
//! - **Pull** drains the repo oplog past this device's cursor. Foreign ops
//!   (signature-verified against the device registry) name the blobs they
//!   wrote or tombstoned; only those blobs are fetched, decrypted, and
//!   materialized. A locally modified file is never overwritten or deleted
//!   by a pull — **local wins**, and the push half re-uploads it, so
//!   divergence converges to the most recent writer without data loss.
//! - **Push** diffs the on-disk scan (the `tuskd export` file set, hashed)
//!   against [`crate::sync_state`]: changed files upload into their stable
//!   blob slots, files this device previously synced and then deleted are
//!   tombstoned, the manifest is re-sealed only when the file *set*
//!   changed, and one signed op announces the affected blob names.
//!
//! Op payloads carry blob names only — names the server already stores in
//! its registry — so the worker adds nothing to what the server can see.
//!
//! Rotation (D27) is honored via the state's RMK generation: when it
//! changes, any blob whose name no longer matches the current RMK's
//! derivation is re-encrypted under a fresh DEK at its new name and the
//! old name tombstoned — server-side content re-keyed without needing the
//! plaintext locally, idempotent across devices because names are
//! deterministic.

use crate::sync_cloud::{client, current_rmk, err, sync_dir, vault_files, CloudConfig, MANIFEST};
use crate::sync_state::{self, FileState, SyncState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tusk_core::error::CoreError;
use tusk_core::sync::content_hash;
use tusk_sync::crypto::{KeyTable, RepoMasterKey};
use tusk_sync::{verify_op, CloudClient, CloudProvider, StorageProvider, SyncError};

/// One oplog entry's payload (opaque to the server). Blob names only.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct OpPayload {
    pub v: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub put: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub del: Vec<String>,
    #[serde(default)]
    pub manifest: bool,
}

/// What one cycle did, for logging and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CycleReport {
    /// Files written locally from remote changes.
    pub pulled: usize,
    /// Files uploaded (including rotation re-keys).
    pub pushed: usize,
    /// Local files deleted because a remote op tombstoned them.
    pub deleted_local: usize,
    /// Remote blobs tombstoned because this device deleted the file.
    pub deleted_remote: usize,
}

impl CycleReport {
    pub fn is_noop(&self) -> bool {
        *self == CycleReport::default()
    }
}

/// True when this vault has been connected to a cloud repo.
pub fn is_connected(vault: &Path) -> bool {
    sync_dir(vault)
        .join(crate::sync_cloud::CLOUD_CONFIG_FILE)
        .exists()
}

// ---------------------------------------------------------------------------
// The background worker thread
// ---------------------------------------------------------------------------

/// Handle to the daemon's sync worker thread. The thread uses blocking
/// HTTP (the tusk-sync clients are blocking by design), so it lives on a
/// plain std thread, not the tokio runtime.
pub struct SyncWorker {
    stop: std::sync::mpsc::Sender<()>,
}

impl SyncWorker {
    /// Run a cycle immediately, then every `interval`, until stopped.
    pub fn spawn(vault: PathBuf, interval: Duration) -> SyncWorker {
        let (stop, wake) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || loop {
            match cycle(&vault) {
                Ok(report) if !report.is_noop() => eprintln!(
                    "sync: pulled {}, pushed {}, deleted {} local / {} remote",
                    report.pulled, report.pushed, report.deleted_local, report.deleted_remote
                ),
                Ok(_) => {}
                Err(e) => eprintln!("sync: cycle failed (will retry): {e}"),
            }
            // The sender is dropped on shutdown; Disconnected ends the loop.
            match wake.recv_timeout(interval) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                _ => break,
            }
        });
        SyncWorker { stop }
    }

    /// Signal the thread to exit after any in-flight cycle. Deliberately
    /// not joined: a cycle can sit in blocking HTTP for minutes, and an
    /// interrupted cycle is crash-safe anyway (writes are atomic, the
    /// state file is saved only at consistent points, uploads are
    /// idempotent into stable slots) — daemon shutdown must stay prompt.
    pub fn stop(self) {
        drop(self.stop);
    }
}

// ---------------------------------------------------------------------------
// One sync cycle
// ---------------------------------------------------------------------------

struct Session {
    cloud: CloudClient,
    provider: CloudProvider,
    config: CloudConfig,
    rmk: RepoMasterKey,
    generation: i32,
}

fn open_session(vault: &Path) -> Result<Session, CoreError> {
    let (cloud, config) = client(vault)?;
    let (cloud2, _) = client(vault)?;
    let provider = CloudProvider::new(cloud2).map_err(err)?;
    let (rmk, generation) = current_rmk(vault, &cloud, &provider, &config.repo_id)?;
    Ok(Session {
        cloud,
        provider,
        config,
        rmk,
        generation,
    })
}

fn fetch_table(session: &Session) -> Result<Option<KeyTable>, CoreError> {
    match session.provider.get(MANIFEST) {
        Ok(sealed) => Ok(Some(
            KeyTable::open(&sealed, &session.rmk, &session.config.repo_id).map_err(err)?,
        )),
        Err(SyncError::NotFound(_)) => Ok(None),
        Err(e) => Err(err(e)),
    }
}

/// Run one full pull-then-push cycle for a connected vault. Public so the
/// manual verbs and tests drive the exact code the daemon runs.
pub fn cycle(vault: &Path) -> Result<CycleReport, CoreError> {
    let session = open_session(vault)?;
    let mut report = CycleReport::default();

    let mut state = match sync_state::load(&sync_dir(vault))? {
        Some(state) => state,
        None => initial_sync(vault, &session, &mut report)?,
    };

    let mut table = pull_phase(vault, &session, &mut state, &mut report)?;
    push_phase(vault, &session, &mut state, &mut table, &mut report)?;

    state.generation = session.generation;
    sync_state::save(&sync_dir(vault), &state)?;
    Ok(report)
}

/// Manual `tuskd sync push`: the push half only — state-aware and
/// incremental, sharing the worker's exact code path. On a vault with no
/// sync state every local file counts as changed, so a first push after
/// `bootstrap` uploads everything; unlike the pre-D28 snapshot push it
/// reuses the existing manifest, so files that only exist remotely are
/// neither dropped from the manifest nor tombstoned.
pub fn push_only(vault: &Path) -> Result<CycleReport, CoreError> {
    let session = open_session(vault)?;
    let dir = sync_dir(vault);
    let mut state = sync_state::load(&dir)?.unwrap_or_else(|| SyncState {
        generation: session.generation,
        ..SyncState::default()
    });
    let mut report = CycleReport::default();
    let mut table = None;
    push_phase(vault, &session, &mut state, &mut table, &mut report)?;
    state.generation = session.generation;
    sync_state::save(&dir, &state)?;
    Ok(report)
}

/// Manual `tuskd sync pull`: materialize the full manifest, overwriting
/// local copies — the explicit "give me the cloud view" command — and
/// record the result so a running worker doesn't re-push what was just
/// pulled.
pub fn pull_all(vault: &Path) -> Result<usize, CoreError> {
    let session = open_session(vault)?;
    let dir = sync_dir(vault);
    let table = fetch_table(&session)?
        .ok_or_else(|| CoreError::Other("nothing pushed yet (no manifest)".into()))?;
    let mut state = sync_state::load(&dir)?.unwrap_or_else(|| SyncState {
        generation: session.generation,
        ..SyncState::default()
    });
    let mut written = 0usize;
    for (rel, entry) in table.iter() {
        let blob = session.provider.get(&entry.blob).map_err(err)?;
        let plaintext = entry
            .dek()
            .map_err(err)?
            .decrypt(&session.config.repo_id, rel, &blob)
            .map_err(err)?;
        write_atomic(vault, &vault.join(rel), &plaintext)?;
        state.files.insert(
            rel.clone(),
            FileState {
                hash: content_hash(&plaintext),
                blob: entry.blob.clone(),
            },
        );
        written += 1;
    }
    sync_state::save(&dir, &state)?;
    Ok(written)
}

/// First cycle on this device: adopt the cloud's current view. Remote
/// files missing locally are materialized; a local file that differs from
/// its remote copy is recorded at the *remote* hash, which marks it dirty
/// so the same cycle's push phase uploads it (local wins, nothing lost).
/// The cursor fast-forwards past history — the manifest already reflects
/// every historical op.
fn initial_sync(
    vault: &Path,
    session: &Session,
    report: &mut CycleReport,
) -> Result<SyncState, CoreError> {
    let mut state = SyncState {
        cursor: latest_seq(&session.cloud)?,
        generation: session.generation,
        files: BTreeMap::new(),
    };
    if let Some(table) = fetch_table(session)? {
        for (rel, entry) in table.iter() {
            let blob = session.provider.get(&entry.blob).map_err(err)?;
            let plaintext = entry
                .dek()
                .map_err(err)?
                .decrypt(&session.config.repo_id, rel, &blob)
                .map_err(err)?;
            let remote_hash = content_hash(&plaintext);
            let path = vault.join(rel);
            if !path.exists() {
                write_atomic(vault, &path, &plaintext)?;
                report.pulled += 1;
            }
            state.files.insert(
                rel.clone(),
                FileState {
                    hash: remote_hash,
                    blob: entry.blob.clone(),
                },
            );
        }
    }
    sync_state::save(&sync_dir(vault), &state)?;
    Ok(state)
}

/// Highest seq currently in the oplog (0 when empty), drained in batches.
fn latest_seq(cloud: &CloudClient) -> Result<i64, CoreError> {
    let mut seq = 0i64;
    loop {
        let batch = cloud.ops_since(seq, Some(500)).map_err(err)?;
        match batch.last() {
            Some(op) => seq = op.seq,
            None => return Ok(seq),
        }
    }
}

/// Apply foreign ops past the cursor. Returns the manifest table if this
/// phase had to fetch it, so the push phase can reuse it.
fn pull_phase(
    vault: &Path,
    session: &Session,
    state: &mut SyncState,
    report: &mut CycleReport,
) -> Result<Option<KeyTable>, CoreError> {
    let ops = session.cloud.ops_since(state.cursor, None).map_err(err)?;
    if ops.is_empty() {
        return Ok(None);
    }

    // Collapse the batch into net per-blob effects, verifying authorship.
    let mut device_keys = DeviceKeys::default();
    let mut puts: BTreeSet<String> = BTreeSet::new();
    let mut dels: BTreeSet<String> = BTreeSet::new();
    let mut manifest_changed = false;
    for op in &ops {
        state.cursor = op.seq;
        if op.device_id == session.config.device_id {
            continue; // our own op; its effects are already in the state
        }
        let key = device_keys.get(&session.cloud, &op.device_id)?;
        verify_op(&key, &session.config.repo_id, &op.payload, &op.signature).map_err(|_| {
            CoreError::Other(format!(
                "sync: op {} claims device {} but its signature does not verify",
                op.seq, op.device_id
            ))
        })?;
        let payload: OpPayload = serde_json::from_slice(&op.payload)
            .map_err(|e| CoreError::Other(format!("sync: op {} payload: {e}", op.seq)))?;
        if payload.v != 1 {
            return Err(CoreError::Other(format!(
                "sync: op {} has payload version {} — upgrade tuskd",
                op.seq, payload.v
            )));
        }
        manifest_changed |= payload.manifest;
        for blob in payload.put {
            dels.remove(&blob);
            puts.insert(blob);
        }
        for blob in payload.del {
            puts.remove(&blob);
            dels.insert(blob);
        }
    }
    if puts.is_empty() && dels.is_empty() && !manifest_changed {
        return Ok(None);
    }

    let table = fetch_table(session)?.unwrap_or_default();

    // Puts: fetch, decrypt, and write — unless the local copy has diverged
    // from the last synced state, in which case local wins and the push
    // phase re-uploads it.
    let by_blob: BTreeMap<&str, &str> = table
        .iter()
        .map(|(rel, entry)| (entry.blob.as_str(), rel.as_str()))
        .collect();
    for blob_name in &puts {
        let Some(rel) = by_blob.get(blob_name.as_str()).copied() else {
            continue; // deleted or re-keyed after this op; a later op covers it
        };
        let entry = match table.get(rel) {
            Some(entry) => entry,
            None => continue,
        };
        let blob = session.provider.get(blob_name).map_err(err)?;
        let plaintext = entry
            .dek()
            .map_err(err)?
            .decrypt(&session.config.repo_id, rel, &blob)
            .map_err(err)?;
        let remote_hash = content_hash(&plaintext);
        let path = vault.join(rel);
        let local = read_optional(&path)?;
        let clean = match (&local, state.files.get(rel)) {
            (None, _) => true,
            (Some(bytes), Some(known)) => content_hash(bytes) == known.hash,
            (Some(_), None) => false, // local file the cloud doesn't know we have
        };
        if let Some(bytes) = &local {
            if content_hash(bytes) == remote_hash {
                // Same content both sides; just record agreement.
                state.files.insert(
                    rel.to_string(),
                    FileState {
                        hash: remote_hash,
                        blob: blob_name.clone(),
                    },
                );
                continue;
            }
        }
        if clean {
            write_atomic(vault, &path, &plaintext)?;
            state.files.insert(
                rel.to_string(),
                FileState {
                    hash: remote_hash,
                    blob: blob_name.clone(),
                },
            );
            report.pulled += 1;
        } else {
            eprintln!("sync: {rel} changed both locally and remotely — keeping the local copy");
        }
    }

    // Dels: delete the local file only when it matches the last synced
    // state; a dirty local copy survives and is re-pushed.
    for blob_name in &dels {
        let Some(rel) = state.rel_for_blob(blob_name).map(str::to_string) else {
            continue; // this device never synced that blob
        };
        if table.get(&rel).is_some() {
            continue; // re-keyed or re-created under the same rel; put path handles it
        }
        let path = vault.join(&rel);
        let clean = match (read_optional(&path)?, state.files.get(&rel)) {
            (Some(bytes), Some(known)) => content_hash(&bytes) == known.hash,
            (None, _) => true,
            _ => false,
        };
        if clean {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| CoreError::io(path.display().to_string(), e))?;
                report.deleted_local += 1;
            }
            state.files.remove(&rel);
        } else {
            eprintln!("sync: {rel} was deleted remotely but modified locally — keeping it");
            state.files.remove(&rel); // now local-only; push re-uploads it
        }
    }

    // A foreign manifest write may have raced ours and dropped entries we
    // still have on disk (last-writer-wins on the manifest blob). Any such
    // file — present locally, absent from the new manifest, not explicitly
    // deleted — is forgotten from the state so the push phase restores it.
    if manifest_changed {
        let clobbered: Vec<String> = state
            .files
            .iter()
            .filter(|(rel, fs)| {
                table.get(rel).is_none() && !dels.contains(&fs.blob) && vault.join(rel).exists()
            })
            .map(|(rel, _)| rel.clone())
            .collect();
        for rel in clobbered {
            state.files.remove(&rel);
        }
    }

    Ok(Some(table))
}

/// Upload local changes: the scan-vs-state diff, plus any rotation re-key.
fn push_phase(
    vault: &Path,
    session: &Session,
    state: &mut SyncState,
    table_cache: &mut Option<KeyTable>,
    report: &mut CycleReport,
) -> Result<(), CoreError> {
    // Scan and hash the sync file set.
    let mut on_disk: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (rel, path) in vault_files(vault)? {
        match std::fs::read(&path) {
            Ok(bytes) => {
                on_disk.insert(rel, bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // raced a delete
            Err(e) => return Err(CoreError::io(path.display().to_string(), e)),
        }
    }

    let changed: Vec<&String> = on_disk
        .keys()
        .filter(|rel| match state.files.get(*rel) {
            Some(known) => content_hash(&on_disk[*rel]) != known.hash,
            None => true,
        })
        .collect();
    let deleted: Vec<String> = state
        .files
        .keys()
        .filter(|rel| !on_disk.contains_key(*rel))
        .cloned()
        .collect();
    let rekey_needed = state.generation != session.generation && state.generation != 0;

    if changed.is_empty() && deleted.is_empty() && !rekey_needed {
        return Ok(());
    }

    let mut table = match table_cache.take() {
        Some(table) => table,
        None => fetch_table(session)?.unwrap_or_default(),
    };
    let mut manifest_dirty = false;
    let mut put_names: Vec<String> = Vec::new();
    let mut del_names: Vec<String> = Vec::new();

    for rel in changed {
        let bytes = &on_disk[rel];
        if table.get(rel).is_none() {
            manifest_dirty = true;
        }
        let entry = table.entry(&session.rmk, rel).map_err(err)?;
        let blob = entry
            .dek()
            .map_err(err)?
            .encrypt(&session.config.repo_id, rel, bytes)
            .map_err(err)?;
        session.provider.put(&entry.blob, &blob).map_err(err)?;
        put_names.push(entry.blob.clone());
        state.files.insert(
            rel.clone(),
            FileState {
                hash: content_hash(bytes),
                blob: entry.blob,
            },
        );
        report.pushed += 1;
    }

    for rel in deleted {
        if let Some(entry) = table.remove(&rel) {
            session.provider.delete(&entry.blob).map_err(err)?;
            del_names.push(entry.blob);
            manifest_dirty = true;
            report.deleted_remote += 1;
        }
        state.files.remove(&rel);
    }

    // Rotation re-key (D27): every manifest entry whose blob name no longer
    // matches the current RMK's derivation is re-encrypted under a fresh
    // DEK at its new name; the old name is tombstoned. Content comes from
    // the old blob itself, so files never pulled here re-key too. Names are
    // deterministic, so devices that sync later find nothing left to do.
    if rekey_needed {
        let stale: Vec<(String, String)> = table
            .iter()
            .filter_map(|(rel, entry)| match session.rmk.blob_name(rel) {
                Ok(expected) if expected != entry.blob => Some((rel.clone(), entry.blob.clone())),
                _ => None,
            })
            .collect();
        for (rel, old_name) in stale {
            let old_entry = table
                .get(&rel)
                .ok_or_else(|| CoreError::Other(format!("re-key lost entry {rel}")))?;
            let blob = session.provider.get(&old_name).map_err(err)?;
            let plaintext = old_entry
                .dek()
                .map_err(err)?
                .decrypt(&session.config.repo_id, &rel, &blob)
                .map_err(err)?;
            table.remove(&rel);
            let entry = table.entry(&session.rmk, &rel).map_err(err)?;
            let sealed = entry
                .dek()
                .map_err(err)?
                .encrypt(&session.config.repo_id, &rel, &plaintext)
                .map_err(err)?;
            session.provider.put(&entry.blob, &sealed).map_err(err)?;
            session.provider.delete(&old_name).map_err(err)?;
            put_names.push(entry.blob.clone());
            del_names.push(old_name);
            if let Some(fs) = state.files.get_mut(&rel) {
                fs.blob = entry.blob;
            }
            manifest_dirty = true;
            report.pushed += 1;
        }
    }

    if manifest_dirty {
        session
            .provider
            .put(
                MANIFEST,
                &table
                    .seal(&session.rmk, &session.config.repo_id)
                    .map_err(err)?,
            )
            .map_err(err)?;
    }

    if !put_names.is_empty() || !del_names.is_empty() {
        let payload = serde_json::to_vec(&OpPayload {
            v: 1,
            put: put_names,
            del: del_names,
            manifest: manifest_dirty,
        })?;
        session.cloud.append_op(&payload).map_err(err)?;
    }

    *table_cache = Some(table);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Cache of device verifying keys for op verification, refreshed once if
/// an op names a device we haven't seen (e.g. enrolled after our last
/// fetch). Revoked devices stay valid as authors of their historical ops.
#[derive(Default)]
struct DeviceKeys {
    keys: BTreeMap<String, ed25519_dalek::VerifyingKey>,
    fetched: bool,
}

impl DeviceKeys {
    fn get(
        &mut self,
        cloud: &CloudClient,
        device_id: &str,
    ) -> Result<ed25519_dalek::VerifyingKey, CoreError> {
        if !self.keys.contains_key(device_id) && !self.fetched {
            self.refresh(cloud)?;
        }
        if !self.keys.contains_key(device_id) {
            self.refresh(cloud)?; // maybe enrolled since; one more look
        }
        self.keys
            .get(device_id)
            .copied()
            .ok_or_else(|| CoreError::Other(format!("sync: op from unknown device {device_id}")))
    }

    fn refresh(&mut self, cloud: &CloudClient) -> Result<(), CoreError> {
        use base64::Engine;
        for device in cloud.list_devices().map_err(err)? {
            let raw: [u8; 32] = base64::engine::general_purpose::STANDARD
                .decode(&device.ed25519_pubkey)
                .map_err(|e| CoreError::Other(format!("bad listed pubkey: {e}")))?
                .try_into()
                .map_err(|_| CoreError::Other("listed pubkey is not 32 bytes".into()))?;
            let key = ed25519_dalek::VerifyingKey::from_bytes(&raw)
                .map_err(|e| CoreError::Other(format!("listed pubkey invalid: {e}")))?;
            self.keys.insert(device.device_id, key);
        }
        self.fetched = true;
        Ok(())
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, CoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CoreError::io(path.display().to_string(), e)),
    }
}

/// tmp + rename in the file's directory, matching the vault's atomic-write
/// posture so the indexer never observes a torn file.
fn write_atomic(vault: &Path, path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(vault);
    std::fs::create_dir_all(parent).map_err(|e| CoreError::io(parent.display().to_string(), e))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CoreError::Other(format!("bad sync path {}", path.display())))?;
    let tmp = parent.join(format!(".{name}.sync-tmp"));
    std::fs::write(&tmp, bytes).map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path.display().to_string(), e))
}

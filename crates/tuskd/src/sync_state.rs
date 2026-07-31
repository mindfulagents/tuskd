//! Device-local sync state for incremental cloud sync (D28):
//! `.tusk/sync/state.json` records, per synced file, the plaintext content
//! hash and blob name as of the last time this device agreed with the
//! cloud about it, plus the oplog cursor (last seq this device has seen).
//!
//! The state is what makes sync incremental *and* safe: pushes are the
//! diff of the on-disk scan against it, deletions propagate only for files
//! this device itself previously synced (a fresh or wiped vault can never
//! mass-tombstone a repo), and a local edit that diverges from it marks
//! the file dirty, which is what lets "local wins" conflict handling work.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tusk_core::error::CoreError;

pub const STATE_FILE: &str = "state.json";

/// One synced file as of the last agreement with the cloud.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    /// sha256 hex of the plaintext (`tusk_core::sync::content_hash`).
    pub hash: String,
    /// Storage blob name (stable across content updates; recorded so ops
    /// that tombstone a blob can be mapped back to the file it named).
    pub blob: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// Last oplog seq this device has applied or knowingly skipped.
    pub cursor: i64,
    /// RMK generation this state was written under; a mismatch with the
    /// current generation triggers the rotation re-key pass (D28).
    #[serde(default)]
    pub generation: i32,
    /// rel_path → last-synced hash + blob name.
    pub files: BTreeMap<String, FileState>,
}

impl SyncState {
    /// Reverse-map a blob name to its rel path, from this device's view.
    pub fn rel_for_blob(&self, blob: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(_, f)| f.blob == blob)
            .map(|(rel, _)| rel.as_str())
    }
}

fn state_path(sync_dir: &Path) -> PathBuf {
    sync_dir.join(STATE_FILE)
}

/// `None` means the device has never completed a sync cycle (triggers the
/// initial-sync path in the worker).
pub fn load(sync_dir: &Path) -> Result<Option<SyncState>, CoreError> {
    let path = state_path(sync_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CoreError::io(path.display().to_string(), e)),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| CoreError::Other(format!("bad {}: {e}", path.display())))
}

/// Atomic write (tmp + rename), matching the vault's crash posture.
pub fn save(sync_dir: &Path, state: &SyncState) -> Result<(), CoreError> {
    crate::platform::create_private_dir(sync_dir)?;
    let path = state_path(sync_dir);
    let tmp = sync_dir.join(format!(".{STATE_FILE}.tmp"));
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| CoreError::Other(format!("serialize sync state: {e}")))?;
    crate::platform::write_private(&tmp, &json)?;
    std::fs::rename(&tmp, &path).map_err(|e| CoreError::io(path.display().to_string(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_blob_reverse_map() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);

        let mut state = SyncState {
            cursor: 42,
            ..SyncState::default()
        };
        state.files.insert(
            "memory/org/a.md".into(),
            FileState {
                hash: "aa".into(),
                blob: "b1".into(),
            },
        );
        save(dir.path(), &state).unwrap();
        let loaded = load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, state);
        assert_eq!(loaded.rel_for_blob("b1"), Some("memory/org/a.md"));
        assert_eq!(loaded.rel_for_blob("b2"), None);
    }
}

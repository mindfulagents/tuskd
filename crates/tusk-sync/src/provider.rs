//! `StorageProvider` — put/get/delete/list of opaque named blobs — plus the
//! filesystem-backed `LocalProvider` (tests, offline use).
//!
//! The trait is **blocking** (D22): every vault/journal seam in tusk-core is
//! synchronous `std::fs`, tokio exists only inside the tuskd daemon's admin
//! plane, and the M1 sync worker runs on its own thread (graduation-timer
//! pattern, D6) where blocking I/O is the natural fit. `reqwest::blocking`
//! was already a workspace dependency.

use crate::error::SyncError;
use std::fs;
use std::path::{Path, PathBuf};

/// Blob store seen from the client. Names are opaque provider keys (in
/// practice `HMAC(RMK, rel_path)` hex plus the `manifest` blob — see
/// `crypto`); values are ciphertext. Implementations must be safe to share
/// across threads (the M1 worker holds one behind an `Arc`).
///
/// Semantics all implementations honor:
/// - `put` overwrites atomically from the reader's point of view;
/// - `get` of an absent name is `SyncError::NotFound`;
/// - `delete` is idempotent (deleting an absent name is `Ok`), matching
///   S3/Spaces `DeleteObject`;
/// - `list` returns names sorted ascending.
pub trait StorageProvider: Send + Sync {
    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError>;
    fn get(&self, name: &str) -> Result<Vec<u8>, SyncError>;
    fn delete(&self, name: &str) -> Result<(), SyncError>;
    fn list(&self) -> Result<Vec<String>, SyncError>;
}

/// Reject anything that could act as a path or URL component. Our own names
/// are lowercase hex (64 chars) or short literals like `manifest`; keep the
/// charset conservative: `[a-z0-9._-]`, no leading dot, length 1..=128.
pub fn validate_name(name: &str) -> Result<(), SyncError> {
    let ok_len = !name.is_empty() && name.len() <= 128;
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !ok_len || !ok_chars || name.starts_with('.') {
        return Err(SyncError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// Directory-backed provider: one file per blob under `root`. Writes are
/// atomic (tmp + rename), matching the vault's write posture.
pub struct LocalProvider {
    root: PathBuf,
}

impl LocalProvider {
    /// Create the root directory if needed and return the provider.
    pub fn new(root: &Path) -> Result<LocalProvider, SyncError> {
        fs::create_dir_all(root).map_err(|e| SyncError::io(root.display(), e))?;
        Ok(LocalProvider {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, name: &str) -> Result<PathBuf, SyncError> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }
}

impl StorageProvider for LocalProvider {
    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError> {
        let path = self.blob_path(name)?;
        let tmp = self.root.join(format!(".tmp-{name}"));
        fs::write(&tmp, bytes).map_err(|e| SyncError::io(tmp.display(), e))?;
        fs::rename(&tmp, &path).map_err(|e| SyncError::io(path.display(), e))?;
        Ok(())
    }

    fn get(&self, name: &str) -> Result<Vec<u8>, SyncError> {
        let path = self.blob_path(name)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(SyncError::NotFound(name.to_string()))
            }
            Err(e) => Err(SyncError::io(path.display(), e)),
        }
    }

    fn delete(&self, name: &str) -> Result<(), SyncError> {
        let path = self.blob_path(name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SyncError::io(path.display(), e)),
        }
    }

    fn list(&self) -> Result<Vec<String>, SyncError> {
        let entries =
            fs::read_dir(&self.root).map_err(|e| SyncError::io(self.root.display(), e))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| SyncError::io(self.root.display(), e))?;
            let is_file = entry
                .file_type()
                .map_err(|e| SyncError::io(self.root.display(), e))?
                .is_file();
            if !is_file {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if !name.starts_with('.') {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }
}

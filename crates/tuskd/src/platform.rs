//! Platform-specific defaults live here and only here (build-loop §0).
//! On Windows later: named pipe + a lock strategy behind these same seams.

use fs2::FileExt;
use sha2::digest::Digest;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tusk_core::error::CoreError;

/// Default Unix-domain-socket path for a vault. `sun_path` is ~104 bytes on
/// macOS, so deep vault paths fall back to a hashed /tmp socket that both
/// daemon and clients derive identically.
pub fn default_uds_path(vault: &Path) -> PathBuf {
    let candidate = vault.join(".tusk").join("tuskd.sock");
    if candidate.as_os_str().len() <= 100 {
        return candidate;
    }
    let digest = sha2::Sha256::digest(vault.as_os_str().as_encoded_bytes());
    PathBuf::from(format!("/tmp/tuskd-{}.sock", hex::encode(&digest[..8])))
}

/// Advisory vault lock (`.tusk/lock`, DECISIONS D9): flock(2), released
/// automatically when the owning process dies. Hold the returned handle for
/// the lifetime of core ownership.
pub struct VaultLock {
    _file: File,
    path: PathBuf,
}

/// Write a file readable only by the owning user (0600 on Unix). Used for
/// the dashboard operator token.
pub fn write_private(path: &Path, contents: &str) -> Result<(), CoreError> {
    std::fs::write(path, contents).map_err(|e| CoreError::io(path.display().to_string(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| CoreError::io(path.display().to_string(), e))?;
    }
    Ok(())
}

/// Best-effort "open in the default browser"; failure is not an error.
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

impl VaultLock {
    pub fn acquire(vault: &Path) -> Result<VaultLock, CoreError> {
        let dir = vault.join(".tusk");
        std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        let path = dir.join("lock");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| CoreError::io(path.display().to_string(), e))?;
        file.try_lock_exclusive()
            .map_err(|_| CoreError::Locked(path.display().to_string()))?;
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(VaultLock { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

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

/// Claude Desktop's config file, relative to a home directory. macOS keeps it
/// under Application Support; elsewhere follow the XDG-ish ~/.config layout.
pub fn claude_desktop_config_path(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Claude/claude_desktop_config.json")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".config/Claude/claude_desktop_config.json")
    }
}

/// Spawn `tuskd --vault <vault> start` fully detached (own process group,
/// no inherited stdio), appending stdout/stderr to `log` (D18). Returns the
/// child's pid; the caller is responsible for waiting until it's ready.
pub fn spawn_detached(exe: &Path, vault: &Path, log: &Path) -> Result<u32, CoreError> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
    }
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| CoreError::io(log.display().to_string(), e))?;
    let err = out
        .try_clone()
        .map_err(|e| CoreError::io(log.display().to_string(), e))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--vault")
        .arg(vault)
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|e| CoreError::Other(format!("spawn daemon: {e}")))?;
    Ok(child.id())
}

/// Create a directory accessible only by the owning user (0700 on Unix).
/// Used for the agent private-key store (D17).
pub fn create_private_dir(dir: &Path) -> Result<(), CoreError> {
    std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| CoreError::io(dir.display().to_string(), e))?;
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

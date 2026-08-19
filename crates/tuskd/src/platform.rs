//! Platform-specific defaults live here and only here (build-loop §0).
//! On Windows later: named pipe + a lock strategy behind these same seams.

use fs2::FileExt;
use sha2::digest::Digest;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
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

/// Politely terminate a process by pid via /bin/kill (no libc, keeps the
/// no-unsafe rule). Used only as the `tuskd stop` fallback for daemons too
/// old to know the UDS `shutdown` verb (D18). SIGTERM triggers the same
/// graceful path in every daemon version shipped.
pub fn terminate_pid(pid: u32) -> Result<(), CoreError> {
    let out = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .map_err(|e| CoreError::Other(format!("kill: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(CoreError::Other(format!(
            "kill -TERM {pid} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
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
        if file.try_lock_exclusive().is_err() {
            // Name the holder (D40): "locked" alone sends people hunting
            // with lsof; the pid is right there in the file.
            let detail = match describe_lock_holder(vault) {
                Some(holder) => format!("{} — held by {holder}", path.display()),
                None => path.display().to_string(),
            };
            return Err(CoreError::Locked(detail));
        }
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(VaultLock { _file: file, path })
    }

    /// Acquire, and if another process has the vault, ask it to yield
    /// (D40): leave a handoff request and keep trying for `wait`. Embedded
    /// `tuskd mcp` sessions watch for the request and release the lock;
    /// anything else (an export, a stuck process) simply lets the wait run
    /// out, and the error names it.
    pub fn acquire_or_request(vault: &Path, wait: Duration) -> Result<VaultLock, CoreError> {
        let first = match VaultLock::acquire(vault) {
            Err(CoreError::Locked(detail)) => detail,
            other => return other,
        };
        let request = handoff_request_path(vault);
        if std::fs::write(&request, format!("{}\n", std::process::id())).is_err() {
            return Err(CoreError::Locked(first));
        }
        let deadline = Instant::now() + wait;
        let result = loop {
            std::thread::sleep(Duration::from_millis(100));
            match VaultLock::acquire(vault) {
                Err(CoreError::Locked(detail)) if Instant::now() < deadline => {
                    let _ = detail;
                }
                other => break other,
            }
        };
        // Whoever yielded is watching this file to learn the outcome:
        // gone + lock free means "the borrower finished, take it back".
        let _ = std::fs::remove_file(&request);
        result
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// D40: where a process that wants the vault asks the current holder to
/// yield. Contents are the requester's pid (informational).
pub fn handoff_request_path(vault: &Path) -> PathBuf {
    vault.join(".tusk").join("handoff")
}

/// Pid recorded in `.tusk/lock` by the process holding it.
pub fn lock_holder_pid(vault: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(vault.join(".tusk").join("lock")).ok()?;
    raw.split_whitespace().next()?.parse().ok()
}

/// "pid 17378 (tuskd --vault … mcp --agent claude-desktop)" for the process
/// holding the vault lock, as far as the platform can tell.
pub fn describe_lock_holder(vault: &Path) -> Option<String> {
    let pid = lock_holder_pid(vault)?;
    let cmd = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    Some(match cmd {
        Some(cmd) => format!("pid {pid} ({cmd})"),
        None => format!("pid {pid}"),
    })
}

// ── Start-at-login service (DECISIONS D39) ───────────────────────────
//
// Platform branching for the login service lives here and only here
// (build-loop §0). macOS gets a LaunchAgent, Linux a systemd --user unit.

/// Reverse-DNS label / unit name shared by both platforms.
pub const SERVICE_LABEL: &str = "ai.opentusk.tuskd";

/// Where the login-service definition lives for this platform.
pub fn service_unit_path(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".config/systemd/user/tuskd.service")
    }
}

/// The unit/plist body. The vault path is baked in: the daemon started at
/// login has no working directory to infer it from.
pub fn service_unit_contents(exe: &Path, vault: &Path) -> String {
    let exe = exe.display();
    let vault = vault.display();
    #[cfg(target_os = "macos")]
    {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--vault</string>
    <string>{vault}</string>
    <string>start</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{vault}/.tusk/daemon.log</string>
  <key>StandardErrorPath</key><string>{vault}/.tusk/daemon.log</string>
</dict>
</plist>
"#
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!(
            "[Unit]\n\
             Description=tuskd (OpenTusk daemon)\n\
             After=default.target\n\n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe} --vault {vault} start\n\
             Restart=on-failure\n\
             StandardOutput=append:{vault}/.tusk/daemon.log\n\
             StandardError=append:{vault}/.tusk/daemon.log\n\n\
             [Install]\n\
             WantedBy=default.target\n"
        )
    }
}

/// Register the unit with the platform's session manager.
pub fn service_load(unit: &Path) -> Result<(), CoreError> {
    #[cfg(target_os = "macos")]
    {
        let target = format!("gui/{}", current_uid()?);
        run_tool(
            "launchctl",
            &["bootstrap", &target, &unit.display().to_string()],
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = unit;
        run_tool("systemctl", &["--user", "daemon-reload"])?;
        run_tool("systemctl", &["--user", "enable", "--now", "tuskd.service"])
    }
}

/// Unregister it. Absent units are not an error — uninstall is idempotent.
pub fn service_unload(unit: &Path) -> Result<(), CoreError> {
    #[cfg(target_os = "macos")]
    {
        let target = format!("gui/{}/{SERVICE_LABEL}", current_uid()?);
        let _ = run_tool("launchctl", &["bootout", &target]);
        let _ = unit;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = unit;
        let _ = run_tool(
            "systemctl",
            &["--user", "disable", "--now", "tuskd.service"],
        );
        let _ = run_tool("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }
}

/// Human-readable hint naming the session manager, for `service status`.
pub fn service_manager_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "systemd --user"
    }
}

/// Current uid via `id -u`. The crate is `#![forbid(unsafe_code)]`, so no libc.
#[cfg(target_os = "macos")]
fn current_uid() -> Result<String, CoreError> {
    let out = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| CoreError::io("id".to_string(), e))?;
    if !out.status.success() {
        return Err(CoreError::Other(
            "could not determine uid via `id -u`".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_tool(bin: &str, args: &[&str]) -> Result<(), CoreError> {
    let out = std::process::Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| CoreError::io(bin.to_string(), e))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(CoreError::Other(format!(
        "{bin} {} failed: {}",
        args.join(" "),
        if stderr.is_empty() {
            out.status.to_string()
        } else {
            stderr
        }
    )))
}

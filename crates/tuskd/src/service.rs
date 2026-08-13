//! Start-at-login service management (DECISIONS D39).
//!
//! A desktop tray has no working directory, so unlike the CLI it cannot infer
//! which vault it manages. This module owns the machine's **single active
//! vault**: the login unit bakes that path in, and `service switch` moves it.
//!
//! Everything here is a thin orchestration over verbs that already exist —
//! `commands::stop_daemon` (graceful, drains over the UDS admin plane and
//! waits for the vault lock to release) and `commands::start_detached`. The
//! tray calls this; it does not reimplement it (D6: one plane).

use crate::platform;
use crate::style::{ACCENT, BOLD, DIM, OK};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tusk_core::error::CoreError;

/// Shared desktop/CLI state. The tray reads and writes the same file, so the
/// active vault has exactly one source of truth.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DesktopState {
    /// Absolute path of the vault the login service manages.
    pub active_vault: Option<PathBuf>,
    /// Most-recently-used vaults, newest first, for the tray's switcher.
    #[serde(default)]
    pub recents: Vec<PathBuf>,
}

/// `~/.config/opentusk/desktop.json` on every platform — the tray ships to
/// macOS and Linux and a single path keeps the two in step.
pub fn state_path(home: &Path) -> PathBuf {
    home.join(".config/opentusk/desktop.json")
}

pub fn load_state(home: &Path) -> Result<DesktopState, CoreError> {
    let path = state_path(home);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| CoreError::Other(format!("{} is not valid JSON: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DesktopState::default()),
        Err(e) => Err(CoreError::io(path.display().to_string(), e)),
    }
}

pub fn save_state(home: &Path, state: &DesktopState) -> Result<(), CoreError> {
    let path = state_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
    }
    let body = serde_json::to_string_pretty(state)
        .map_err(|e| CoreError::Other(format!("serializing desktop state: {e}")))?;
    std::fs::write(&path, format!("{body}\n"))
        .map_err(|e| CoreError::io(path.display().to_string(), e))
}

/// Record `vault` as active, keeping a de-duplicated recents list.
pub fn set_active(state: &mut DesktopState, vault: &Path) {
    let vault = vault.to_path_buf();
    state.recents.retain(|p| p != &vault);
    if let Some(previous) = state.active_vault.replace(vault.clone()) {
        if previous != vault {
            state.recents.retain(|p| p != &previous);
            state.recents.insert(0, previous);
        }
    }
    state.recents.truncate(10);
}

fn absolute(vault: &Path) -> Result<PathBuf, CoreError> {
    std::fs::canonicalize(vault).map_err(|e| CoreError::io(vault.display().to_string(), e))
}

fn require_vault(vault: &Path) -> Result<PathBuf, CoreError> {
    let abs = absolute(vault)?;
    if !abs.join(".tusk").is_dir() {
        return Err(CoreError::Other(format!(
            "{} is not a vault — run `tuskd init` there first",
            abs.display()
        )));
    }
    Ok(abs)
}

fn current_exe() -> Result<PathBuf, CoreError> {
    std::env::current_exe().map_err(|e| CoreError::io("current_exe".to_string(), e))
}

/// `tuskd service install` — write the login unit for `vault` and load it.
pub fn install(vault: &Path, home: &Path) -> Result<(), CoreError> {
    let vault = require_vault(vault)?;
    let exe = current_exe()?;
    let unit = platform::service_unit_path(home);
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
    }
    // Reload rather than stack a second registration if one already exists.
    let _ = platform::service_unload(&unit);
    std::fs::write(&unit, platform::service_unit_contents(&exe, &vault))
        .map_err(|e| CoreError::io(unit.display().to_string(), e))?;

    // Record the active vault *before* asking the session manager to load it.
    // If the load fails (no user session bus, launchd refusing), the unit on
    // disk and the recorded active vault still agree — otherwise `status`
    // would report "none" while a unit sits there pointing at a vault.
    let mut state = load_state(home)?;
    set_active(&mut state, &vault);
    save_state(home, &state)?;

    platform::service_load(&unit)?;

    anstream::println!(
        "{OK}✓{OK:#} start at login enabled ({})",
        platform::service_manager_name()
    );
    anstream::println!("  vault {ACCENT}{}{ACCENT:#}", vault.display());
    anstream::println!("  unit  {DIM}{}{DIM:#}", unit.display());
    Ok(())
}

/// `tuskd service uninstall` — unload and remove the unit. Idempotent; the
/// vault and its data are untouched.
pub fn uninstall(home: &Path) -> Result<(), CoreError> {
    let unit = platform::service_unit_path(home);
    platform::service_unload(&unit)?;
    match std::fs::remove_file(&unit) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anstream::println!("{DIM}no login unit was installed{DIM:#}");
            return Ok(());
        }
        Err(e) => return Err(CoreError::io(unit.display().to_string(), e)),
    }
    let mut state = load_state(home)?;
    state.active_vault = None;
    save_state(home, &state)?;
    anstream::println!("{OK}✓{OK:#} start at login disabled");
    Ok(())
}

/// `tuskd service status` — the active vault is printed first and always.
///
/// Guardrail: the failure mode this whole design risks is silently managing a
/// different vault than the terminal is, so the answer to "which vault?" is
/// the headline, not a footnote.
pub fn status(home: &Path) -> Result<(), CoreError> {
    let unit = platform::service_unit_path(home);
    let state = load_state(home)?;

    match &state.active_vault {
        Some(vault) => {
            anstream::println!(
                "{BOLD}active vault{BOLD:#}  {ACCENT}{}{ACCENT:#}",
                vault.display()
            )
        }
        None => anstream::println!("{BOLD}active vault{BOLD:#}  {DIM}none{DIM:#}"),
    }
    let installed = unit.exists();
    anstream::println!(
        "start at login  {}",
        if installed {
            format!(
                "{OK}unit installed{OK:#} ({})",
                platform::service_manager_name()
            )
        } else {
            format!("{DIM}not installed{DIM:#}")
        }
    );
    anstream::println!("unit            {DIM}{}{DIM:#}", unit.display());
    if !state.recents.is_empty() {
        anstream::println!("{DIM}recent vaults:{DIM:#}");
        for vault in &state.recents {
            anstream::println!("  {DIM}{}{DIM:#}", vault.display());
        }
    }
    Ok(())
}

/// `tuskd service switch <vault>` — move the machine to a different vault.
///
/// Not a view toggle: stop the old daemon (gracefully, so an in-flight sync
/// drains), re-point the login unit, start the new one. Machine-global MCP
/// clients hold either the absolute vault path or a vault-scoped token, so
/// they must be re-pointed too — this reports exactly which, and never
/// rewrites a client config behind the user's back.
pub fn switch(new_vault: &Path, home: &Path) -> Result<(), CoreError> {
    let new_vault = require_vault(new_vault)?;
    let mut state = load_state(home)?;
    let previous = state.active_vault.clone();

    if previous.as_deref() == Some(new_vault.as_path()) {
        anstream::println!(
            "{DIM}already on{DIM:#} {ACCENT}{}{ACCENT:#}",
            new_vault.display()
        );
        return Ok(());
    }

    // 1. Stop the outgoing daemon. stop_daemon shuts down over the UDS admin
    //    plane and waits for the vault lock, which is the drain.
    if let Some(old) = &previous {
        if old.join(".tusk").is_dir() {
            crate::commands::stop_daemon(old, true)?;
        }
    }

    // 2. Re-point the login unit, or a reboot silently reverts to the old vault.
    let unit = platform::service_unit_path(home);
    if unit.exists() {
        install(&new_vault, home)?;
    } else {
        set_active(&mut state, &new_vault);
        save_state(home, &state)?;
        anstream::println!(
            "{DIM}start at login is not installed — run `tuskd service install` to persist this{DIM:#}"
        );
        // 3. Start the new daemon even without a login unit.
        crate::commands::start_detached(&new_vault)?;
    }

    // 3'. With a unit installed, the session manager starts it; make sure a
    //     daemon is actually up either way.
    if unit.exists() {
        crate::commands::start_detached(&new_vault)?;
    }

    anstream::println!(
        "{OK}✓{OK:#} switched to {ACCENT}{}{ACCENT:#}",
        new_vault.display()
    );
    report_clients_needing_repoint(home);
    Ok(())
}

/// Guardrail: name the machine-global clients still pointing at the old vault
/// and the exact command to move each one. Project-scoped clients
/// (`claude-code`, `vscode`) live inside their vault and need nothing.
fn report_clients_needing_repoint(home: &Path) {
    let stale = crate::setup::global_clients_configured(home);
    if stale.is_empty() {
        return;
    }
    anstream::println!(
        "\n{BOLD}these clients still point at the previous vault{BOLD:#} {DIM}(re-point when you want them moved){DIM:#}"
    );
    for (name, next_step) in &stale {
        anstream::println!("  {ACCENT}tuskd agent setup {name}{ACCENT:#}");
        anstream::println!("    {DIM}{next_step}{DIM:#}");
    }
    anstream::println!(
        "{DIM}claude-code and vscode are project-scoped — they travel with their own vault.{DIM:#}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_active_promotes_previous_into_recents() {
        let mut state = DesktopState::default();
        set_active(&mut state, Path::new("/vaults/a"));
        assert_eq!(state.active_vault.as_deref(), Some(Path::new("/vaults/a")));
        assert!(state.recents.is_empty());

        set_active(&mut state, Path::new("/vaults/b"));
        assert_eq!(state.active_vault.as_deref(), Some(Path::new("/vaults/b")));
        assert_eq!(state.recents, vec![PathBuf::from("/vaults/a")]);
    }

    #[test]
    fn set_active_is_idempotent_and_never_duplicates() {
        let mut state = DesktopState::default();
        set_active(&mut state, Path::new("/vaults/a"));
        set_active(&mut state, Path::new("/vaults/a"));
        assert_eq!(state.active_vault.as_deref(), Some(Path::new("/vaults/a")));
        assert!(
            state.recents.is_empty(),
            "re-selecting must not self-shadow"
        );

        set_active(&mut state, Path::new("/vaults/b"));
        set_active(&mut state, Path::new("/vaults/a"));
        assert_eq!(state.recents, vec![PathBuf::from("/vaults/b")]);
    }

    #[test]
    fn recents_are_capped() {
        let mut state = DesktopState::default();
        for i in 0..15 {
            set_active(&mut state, Path::new(&format!("/vaults/{i}")));
        }
        assert!(state.recents.len() <= 10);
    }

    #[test]
    fn state_roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!("tuskd-service-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = DesktopState::default();
        set_active(&mut state, Path::new("/vaults/a"));
        set_active(&mut state, Path::new("/vaults/b"));
        save_state(&dir, &state).unwrap();

        let read = load_state(&dir).unwrap();
        assert_eq!(read.active_vault.as_deref(), Some(Path::new("/vaults/b")));
        assert_eq!(read.recents, vec![PathBuf::from("/vaults/a")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_state_file_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join(format!("tuskd-service-absent-{}", std::process::id()));
        let state = load_state(&dir).unwrap();
        assert!(state.active_vault.is_none());
    }

    #[test]
    fn unit_contents_bake_in_the_vault_path() {
        let body = platform::service_unit_contents(
            Path::new("/usr/local/bin/tuskd"),
            Path::new("/vaults/work"),
        );
        assert!(
            body.contains("/vaults/work"),
            "the login daemon has no cwd to infer from"
        );
        assert!(body.contains("/usr/local/bin/tuskd"));
    }
}

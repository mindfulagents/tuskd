//! `tuskd sync` cloud verbs (M1 D26, M2 D29): login / init / repos /
//! connect / status / devices / approve / push / pull — the CLI face of
//! the proven pieces in `tusk-sync` (CloudClient D23, CloudProvider D24,
//! device flow D25, AccountClient D29).
//!
//! State lives in `.tusk/sync/` (0700; export- and sync-excluded):
//! `cloud.json` (server URL + repo/device ids), `device.pem` (D21 device
//! key, shared with the journal groundwork), and `rmk.hex` (the Repo
//! Master Key — same custody effort level as the device PEMs, D22).
//!
//! Sync semantics (D26, revised by D28): the file set is exactly the
//! `tuskd export` file set; `push` is the worker's incremental path
//! (scan-vs-state diff, stable blob slots, oplog announcement) and `pull`
//! materializes the manifest additively (no local deletions). The daemon
//! runs the same cycle automatically — see `sync_worker`.

use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, DecodePrivateKey, EncodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tusk_core::error::CoreError;
use tusk_sync::crypto::{KeyTable, RepoMasterKey};
use tusk_sync::wrap::{unwrap_rmk_for_device, wrap_rmk_for_device};
use tusk_sync::{CloudClient, CloudProvider, StorageProvider, SyncError};

pub(crate) const CLOUD_CONFIG_FILE: &str = "cloud.json";
const RMK_FILE: &str = "rmk.otsk";
pub(crate) const MANIFEST: &str = "manifest";

/// Persistent connection settings, `.tusk/sync/cloud.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    pub url: String,
    pub repo_id: String,
    pub device_id: String,
}

pub(crate) fn err(e: SyncError) -> CoreError {
    CoreError::Other(format!("sync: {e}"))
}

fn io(path: &Path, e: std::io::Error) -> CoreError {
    CoreError::io(path.display().to_string(), e)
}

pub(crate) fn sync_dir(vault: &Path) -> PathBuf {
    vault.join(".tusk").join("sync")
}

fn load_config(vault: &Path) -> Result<CloudConfig, CoreError> {
    let path = sync_dir(vault).join(CLOUD_CONFIG_FILE);
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        CoreError::Other("not connected — run `tuskd sync init` (or connect) first".into())
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| CoreError::Other(format!("bad {CLOUD_CONFIG_FILE}: {e}")))
}

fn save_config(vault: &Path, config: &CloudConfig) -> Result<(), CoreError> {
    let dir = sync_dir(vault);
    crate::platform::create_private_dir(&dir)?;
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| CoreError::Other(format!("serialize config: {e}")))?;
    crate::platform::write_private(&dir.join(CLOUD_CONFIG_FILE), &json)
}

/// Load-or-create the D21 device key (same file the journal groundwork
/// uses), returning the signing key.
fn ensure_device_key(vault: &Path) -> Result<SigningKey, CoreError> {
    let dir = sync_dir(vault);
    crate::platform::create_private_dir(&dir)?;
    let key_path = dir.join(tusk_core::sync::DEVICE_KEY_FILE);
    let private_pem = if key_path.exists() {
        std::fs::read_to_string(&key_path).map_err(|e| io(&key_path, e))?
    } else {
        let (private_pem, _public_pem) = tusk_core::sync::generate_device_key()?;
        crate::platform::write_private(&key_path, &private_pem)?;
        private_pem
    };
    SigningKey::from_pkcs8_pem(&private_pem)
        .map_err(|e| CoreError::Other(format!("device key: {e}")))
}

pub(crate) fn client(vault: &Path) -> Result<(CloudClient, CloudConfig), CoreError> {
    let config = load_config(vault)?;
    let key = ensure_device_key(vault)?;
    let client =
        CloudClient::new(&config.url, &config.repo_id, &config.device_id, key).map_err(err)?;
    Ok((client, config))
}

const RMK_GEN_FILE: &str = "rmk.gen";

fn load_local_rmk(vault: &Path) -> Option<(RepoMasterKey, i32)> {
    let raw = std::fs::read_to_string(sync_dir(vault).join(RMK_FILE)).ok()?;
    let rmk = RepoMasterKey::from_otsk(raw.trim()).ok()?;
    let generation = std::fs::read_to_string(sync_dir(vault).join(RMK_GEN_FILE))
        .ok()
        .and_then(|g| g.trim().parse().ok())
        .unwrap_or(1);
    Some((rmk, generation))
}

fn store_rmk(vault: &Path, rmk: &RepoMasterKey, generation: i32) -> Result<(), CoreError> {
    crate::platform::write_private(&sync_dir(vault).join(RMK_FILE), &rmk.to_otsk())?;
    crate::platform::write_private(&sync_dir(vault).join(RMK_GEN_FILE), &generation.to_string())
}

/// Fetch this device's wrap and unwrap it locally.
fn rmk_from_own_wrap(
    vault: &Path,
    cloud: &CloudClient,
    repo_id: &str,
) -> Result<(RepoMasterKey, i32), CoreError> {
    let (wrap_bytes, generation) = cloud.fetch_wrap().map_err(|e| match e {
        SyncError::Http { status: 403, .. } => CoreError::Other(
            "this device is not approved (or has been revoked) — check `tuskd sync status`".into(),
        ),
        other => err(other),
    })?;
    let wrap: tusk_sync::DeviceWrap = serde_json::from_slice(&wrap_bytes)
        .map_err(|e| CoreError::Other(format!("bad wrap from server: {e}")))?;
    let key_path = sync_dir(vault).join(tusk_core::sync::DEVICE_KEY_FILE);
    let private_pem = std::fs::read_to_string(&key_path).map_err(|e| io(&key_path, e))?;
    let rmk = unwrap_rmk_for_device(&wrap, repo_id, &private_pem).map_err(err)?;
    Ok((rmk, generation))
}

/// The *current* RMK: the local copy if it still opens the server manifest,
/// else refreshed from this device's own wrap (covering rotations performed
/// elsewhere), persisted on refresh. Generation labels are bookkeeping for
/// wraps; correctness rides on the manifest-open check.
pub(crate) fn current_rmk(
    vault: &Path,
    cloud: &CloudClient,
    provider: &CloudProvider,
    repo_id: &str,
) -> Result<(RepoMasterKey, i32), CoreError> {
    let sealed = match provider.get(MANIFEST) {
        Ok(sealed) => Some(sealed),
        Err(SyncError::NotFound(_)) => None,
        Err(e) => return Err(err(e)),
    };
    if let Some((rmk, generation)) = load_local_rmk(vault) {
        match &sealed {
            None => return Ok((rmk, generation)),
            Some(sealed) if KeyTable::open(sealed, &rmk, repo_id).is_ok() => {
                return Ok((rmk, generation))
            }
            Some(_) => println!("local repo key is stale (rotated elsewhere?) — refreshing"),
        }
    }
    let (rmk, generation) = rmk_from_own_wrap(vault, cloud, repo_id)?;
    if let Some(sealed) = &sealed {
        KeyTable::open(sealed, &rmk, repo_id).map_err(|_| {
            CoreError::Other(
                "this device's wrap does not open the current manifest — a rotation may be \
                 in progress; retry shortly"
                    .into(),
            )
        })?;
    }
    store_rmk(vault, &rmk, generation)?;
    println!("recovered repo key from this device's wrap (generation {generation})");
    Ok((rmk, generation))
}

fn device_keys_raw(key: &SigningKey) -> ([u8; 32], [u8; 32]) {
    let verifying = key.verifying_key();
    (verifying.to_bytes(), verifying.to_montgomery().to_bytes())
}

fn default_name(name: Option<String>) -> String {
    name.unwrap_or_else(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unnamed-device".to_string())
    })
}

/// Basenames that carry no signal as a repo name (D34) — lowercase.
const GENERIC_REPO_NAMES: &[&str] = &["tmp", "temp", "test", "tests", "scratch", "untitled", "new"];

/// D34: does this basename deserve to be a repo name on its own? At
/// least three alphanumeric characters and not a generic junk word — a
/// repo named "a" should only ever happen by explicit choice.
fn looks_like_a_name(s: &str) -> bool {
    s.chars().filter(|c| c.is_alphanumeric()).count() >= 3
        && !GENERIC_REPO_NAMES.contains(&s.to_ascii_lowercase().as_str())
}

/// D34: derive a defensible default repo name from the vault path. The
/// basename wins when it looks like a name; a junk basename gets its
/// parent prefixed for signal (`projects/a` → `projects-a`); the home
/// directory (whose basename is just the username) and unresolvable
/// paths fall back to "vault".
fn derive_repo_name(vault: &Path) -> String {
    let fallback = || "vault".to_string();
    let Ok(canonical) = vault.canonicalize() else {
        return fallback();
    };
    if let Some(home) = std::env::var_os("HOME") {
        if Path::new(&home).canonicalize().ok().as_deref() == Some(&canonical) {
            return fallback();
        }
    }
    let Some(base) = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        return fallback();
    };
    if looks_like_a_name(&base) {
        return base;
    }
    let parent = canonical
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    match parent {
        Some(parent) if looks_like_a_name(&parent) => format!("{parent}-{base}"),
        _ => fallback(),
    }
}

/// D34: ask on a TTY with an editable default (empty answer keeps it);
/// non-interactive runs keep the default silently, so scripts and CI
/// behave exactly as before.
fn prompt_default(label: &str, default: &str) -> Result<String, CoreError> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Ok(default.to_string());
    }
    print!("{label} [{default}]: ");
    std::io::stdout()
        .flush()
        .map_err(|e| CoreError::Other(format!("stdout: {e}")))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| CoreError::Other(format!("read name: {e}")))?;
    let answer = line.trim();
    Ok(if answer.is_empty() {
        default.to_string()
    } else {
        answer.to_string()
    })
}

const SESSION_FILE: &str = "session.json";

/// Persisted account session, `.tusk/sync/session.json` (0600 like every
/// file in the sync dir; the token is a bearer credential — D29).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    url: String,
    token: String,
    email: String,
}

fn load_session(vault: &Path) -> Result<StoredSession, CoreError> {
    let path = sync_dir(vault).join(SESSION_FILE);
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| CoreError::Other("not logged in — run `tuskd sync login` first".into()))?;
    serde_json::from_str(&raw).map_err(|e| CoreError::Other(format!("bad {SESSION_FILE}: {e}")))
}

fn save_session(vault: &Path, session: &StoredSession) -> Result<(), CoreError> {
    let dir = sync_dir(vault);
    crate::platform::create_private_dir(&dir)?;
    let json = serde_json::to_string_pretty(session)
        .map_err(|e| CoreError::Other(format!("serialize session: {e}")))?;
    crate::platform::write_private(&dir.join(SESSION_FILE), &json)
}

fn account_client(vault: &Path) -> Result<(tusk_sync::AccountClient, StoredSession), CoreError> {
    let session = load_session(vault)?;
    let client =
        tusk_sync::AccountClient::new(&session.url, Some(session.token.clone())).map_err(err)?;
    Ok((client, session))
}

fn session_expired(e: SyncError) -> CoreError {
    match e {
        SyncError::Http { status: 401, .. } => {
            CoreError::Other("session expired — run `tuskd sync login` again".into())
        }
        other => err(other),
    }
}

/// `tuskd sync login` — emailed sign-in code → stored session (D29).
pub fn login(vault: &Path, url: &str, email: &str, code: Option<String>) -> Result<(), CoreError> {
    let mut client = tusk_sync::AccountClient::new(url, None).map_err(err)?;
    let code = match code {
        Some(code) => code,
        None => {
            client.auth_start(email).map_err(|e| match e {
                SyncError::Http { status: 429, .. } => CoreError::Other(
                    "too many sign-in codes requested for this email — wait a bit".into(),
                ),
                other => err(other),
            })?;
            println!("sign-in code sent to {email} — enter it below");
            print!("code: ");
            use std::io::Write;
            std::io::stdout()
                .flush()
                .map_err(|e| CoreError::Other(format!("stdout: {e}")))?;
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|e| CoreError::Other(format!("read code: {e}")))?;
            line.trim().to_string()
        }
    };
    let session = client
        .auth_verify(email, &code, &default_name(None))
        .map_err(|e| match e {
            SyncError::Http { status: 401, .. } => CoreError::Other(
                "code rejected (wrong, expired, or used) — request a new one".into(),
            ),
            other => err(other),
        })?;
    save_session(
        vault,
        &StoredSession {
            url: url.trim_end_matches('/').to_string(),
            token: session.token.clone(),
            email: session.email.clone(),
        },
    )?;
    println!("logged in as {} ({} plan)", session.email, session.plan);
    crate::style::hint("next: tuskd sync init   # create a cloud repo for this vault");
    Ok(())
}

/// The recovery-phrase ceremony (D35): a gold box with the phrase in
/// tidy plain columns on a TTY; the exact pre-D35 lines when piped.
/// The words themselves never carry styles — they must copy clean.
fn print_recovery_phrase(phrase: &str, rotated: bool) {
    if !crate::style::stdout_is_tty() {
        if rotated {
            println!("NEW RECOVERY PHRASE — the old one is now useless; write this down:");
        } else {
            println!("RECOVERY PHRASE — write this down; it is shown exactly once:");
        }
        println!("  {phrase}");
        return;
    }
    for line in recovery_phrase_box(phrase, rotated) {
        anstream::println!("{line}");
    }
}

/// The styled lines of the ceremony box. Widths are computed in chars
/// (every char used is single-column; `len()` would over-pad the em-dash
/// titles). The phrase words carry no styles — they must copy clean.
fn recovery_phrase_box(phrase: &str, rotated: bool) -> Vec<String> {
    use crate::style::{ACCENT, ERR};
    let title = if rotated {
        "NEW RECOVERY PHRASE — the old phrase is now useless"
    } else {
        "RECOVERY PHRASE — shown exactly once"
    };
    let warning = [
        "Anyone with these words can decrypt your vault.",
        "Write them down and store them offline.",
    ];
    let width = |s: &str| s.chars().count();
    let words: Vec<&str> = phrase.split_whitespace().collect();
    let cell = words.iter().map(|w| width(w)).max().unwrap_or(8);
    let rows: Vec<String> = words
        .chunks(4)
        .map(|chunk| {
            chunk
                .iter()
                .map(|w| format!("{w:<cell$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect();
    let inner = width(title)
        .max(warning.iter().map(|w| width(w)).max().unwrap_or(0))
        .max(rows.iter().map(|r| width(r) + 2).max().unwrap_or(0))
        + 4;
    let pad = |text: &str, indent: usize| " ".repeat(inner - indent - width(text));
    let mut out = Vec::new();
    out.push(format!("{ACCENT}┌{}┐{ACCENT:#}", "─".repeat(inner)));
    out.push(format!(
        "{ACCENT}│{ACCENT:#}  {ERR}{title}{ERR:#}{}{ACCENT}│{ACCENT:#}",
        pad(title, 2)
    ));
    out.push(format!("{ACCENT}│{}│{ACCENT:#}", " ".repeat(inner)));
    for row in &rows {
        out.push(format!(
            "{ACCENT}│{ACCENT:#}    {row}{}{ACCENT}│{ACCENT:#}",
            pad(row, 4)
        ));
    }
    out.push(format!("{ACCENT}│{}│{ACCENT:#}", " ".repeat(inner)));
    for line in warning {
        out.push(format!(
            "{ACCENT}│{ACCENT:#}  {line}{}{ACCENT}│{ACCENT:#}",
            pad(line, 2)
        ));
    }
    out.push(format!("{ACCENT}└{}┘{ACCENT:#}", "─".repeat(inner)));
    out
}

/// `tuskd sync init` — create a repo under the logged-in account with this
/// vault's device as its first approved device; the self-serve successor
/// to `sync bootstrap` (D29).
pub fn init(
    vault: &Path,
    repo_name: Option<String>,
    name: Option<String>,
) -> Result<(), CoreError> {
    let (client, session) = account_client(vault)?;
    let repo_name = match repo_name {
        Some(name) => name,
        // D34: derive a guard-railed default and confirm it on a TTY
        // instead of silently committing whatever the folder is called.
        None => prompt_default("Repo name", &derive_repo_name(vault))?,
    };
    let key = ensure_device_key(vault)?;
    let (ed25519, x25519) = device_keys_raw(&key);
    let (repo_id, device_id) = client
        .create_repo(&repo_name, &default_name(name), &ed25519, &x25519)
        .map_err(|e| match e {
            SyncError::Http { status: 403, .. } => CoreError::Other(
                "repo limit reached for your plan (or session lacks access)".into(),
            ),
            SyncError::Http { status: 409, .. } => {
                CoreError::Other(format!("you already have a repo named {repo_name:?}"))
            }
            other => session_expired(other),
        })?;
    save_config(
        vault,
        &CloudConfig {
            url: session.url.clone(),
            repo_id: repo_id.clone(),
            device_id,
        },
    )?;
    let rmk = RepoMasterKey::generate();
    store_rmk(vault, &rmk, 1)?;
    println!("repo created: {repo_id} ({repo_name})");
    println!();
    print_recovery_phrase(&rmk.to_mnemonic().map_err(err)?, false);
    println!();
    crate::style::hint("next: tuskd sync push   # or just run the daemon — it syncs automatically");
    Ok(())
}

/// `tuskd sync repos` — list the account's repos (D29).
pub fn repos(vault: &Path) -> Result<(), CoreError> {
    let (client, session) = account_client(vault)?;
    let repos = client.list_repos().map_err(session_expired)?;
    if repos.is_empty() {
        println!("no repos yet — run `tuskd sync init`");
        return Ok(());
    }
    println!("repos of {}:", session.email);
    if crate::style::stdout_is_tty() {
        // Aligned table (D35); ✓ marks the repo this vault is connected to.
        use crate::style::{DIM, OK};
        let connected = load_config(vault).ok().map(|c| c.repo_id);
        let id_width = repos.iter().map(|r| r.repo_id.len()).max().unwrap_or(2);
        anstream::println!("  {DIM}{:<id_width$}  GEN  NAME{DIM:#}", "ID");
        for repo in repos {
            let mark = if connected.as_deref() == Some(repo.repo_id.as_str()) {
                format!("{OK}✓{OK:#}")
            } else {
                " ".to_string()
            };
            anstream::println!(
                "{mark} {:<id_width$}  {:<3}  {}",
                repo.repo_id,
                repo.rmk_generation,
                repo.name
            );
        }
    } else {
        for repo in repos {
            println!(
                "{}  gen {}  {}",
                repo.repo_id, repo.rmk_generation, repo.name
            );
        }
    }
    Ok(())
}

/// `tuskd sync delete-repo` — permanently delete an owned cloud repo
/// (D31). Cloud-side only: local files stay; if this vault was connected
/// to the deleted repo its connection state is cleared so `init` or
/// `connect` starts clean.
pub fn delete_repo(vault: &Path, repo_id: &str, yes: bool) -> Result<(), CoreError> {
    if !yes {
        return Err(CoreError::Other(
            "deleting a cloud repo is permanent (every device loses the cloud copy; \
             local vaults are untouched) — rerun with --yes to confirm"
                .into(),
        ));
    }
    let (client, _session) = account_client(vault)?;
    client.delete_repo(repo_id).map_err(|e| match e {
        SyncError::Http { status: 404, .. } => {
            CoreError::Other("no such repo on your account".into())
        }
        other => session_expired(other),
    })?;
    println!("deleted cloud repo {repo_id}");
    if let Ok(config) = load_config(vault) {
        if config.repo_id == repo_id {
            let _ = std::fs::remove_file(sync_dir(vault).join(CLOUD_CONFIG_FILE));
            println!("this vault was connected to it — connection cleared (files kept)");
        }
    }
    Ok(())
}

/// `tuskd sync rename` — rename a cloud repo (D34; server side is
/// tusk-cloud C15). Display metadata only: the repo id, devices, and
/// key material are untouched. Defaults to the repo this vault is
/// connected to; `--repo` targets any owned repo.
pub fn rename(vault: &Path, name: &str, repo: Option<String>) -> Result<(), CoreError> {
    let (client, _session) = account_client(vault)?;
    let repo_id = match repo {
        Some(id) => id,
        None => load_config(vault)
            .map(|config| config.repo_id)
            .map_err(|_| {
                CoreError::Other(
                    "this vault is not connected to a repo — pass --repo <id> \
                     (see `tuskd sync repos`)"
                        .into(),
                )
            })?,
    };
    client.rename_repo(&repo_id, name).map_err(|e| match e {
        SyncError::Http { status: 404, .. } => {
            CoreError::Other("no such repo on your account".into())
        }
        SyncError::Http { status: 409, .. } => {
            CoreError::Other(format!("you already have a repo named {name:?}"))
        }
        SyncError::Http { status: 400, .. } => {
            CoreError::Other("repo name must be 1-100 characters".into())
        }
        other => session_expired(other),
    })?;
    println!("renamed repo {repo_id} to {name:?}");
    Ok(())
}

/// Default control-plane URL, shared by `login` (clap default) and
/// `connect`'s inference (D33).
pub const DEFAULT_CLOUD_URL: &str = "https://cloud.opentusk.ai";

/// `tuskd sync connect` — enroll this vault's device into an existing
/// repo. The URL is inferred when omitted (D33): the login session's
/// server first, else the stock default — `connect` right after
/// `login`/`repos` needs no flag.
pub fn connect(
    vault: &Path,
    url: Option<String>,
    repo_id: &str,
    name: Option<String>,
    phrase: Option<String>,
) -> Result<(), CoreError> {
    let url = &match url {
        Some(url) => url,
        None => match load_session(vault) {
            Ok(session) => {
                println!("using {} (from your login session)", session.url);
                session.url
            }
            Err(_) => DEFAULT_CLOUD_URL.to_string(),
        },
    };
    let key = ensure_device_key(vault)?;
    let (ed25519, x25519) = device_keys_raw(&key);
    let (device_id, fingerprint) =
        tusk_sync::enroll_device(url, repo_id, &default_name(name), &ed25519, &x25519)
            .map_err(err)?;
    save_config(
        vault,
        &CloudConfig {
            url: url.trim_end_matches('/').to_string(),
            repo_id: repo_id.to_string(),
            device_id: device_id.clone(),
        },
    )?;
    if let Some(phrase) = phrase {
        let rmk = RepoMasterKey::from_mnemonic(&phrase).map_err(err)?;
        store_rmk(vault, &rmk, 1)?;
        println!("repo key recovered from phrase and stored");
    }
    println!("enrolled as device {device_id} (pending)");
    if crate::style::stdout_is_tty() {
        // The approval ceremony (D35): the fingerprint in the same gold
        // 4-char groups the dashboard's devices screen shows, so checking
        // it across the two surfaces is a visual diff. The copyable
        // command below keeps the raw form.
        use crate::style::{fingerprint_groups, ACCENT, DIM};
        println!();
        anstream::println!(
            "  fingerprint   {ACCENT}{}{ACCENT:#}",
            fingerprint_groups(&fingerprint)
        );
        println!();
        println!("approve from an already-approved machine:");
        println!("  tuskd sync approve {device_id} --fingerprint {fingerprint}");
        anstream::println!(
            "{DIM}or from the dashboard's devices page — approve only if the fingerprint\n\
             there matches this screen exactly.{DIM:#}"
        );
    } else {
        println!("fingerprint: {fingerprint}");
        println!(
            "on an approved device, run: tuskd sync approve {device_id} --fingerprint {fingerprint}"
        );
    }
    Ok(())
}

/// This vault's cloud connection, if configured (probe for the wizard).
pub(crate) fn connection(vault: &Path) -> Option<CloudConfig> {
    load_config(vault).ok()
}

/// The logged-in account email, if a session is stored (wizard probe).
pub(crate) fn session_email(vault: &Path) -> Option<String> {
    load_session(vault).ok().map(|s| s.email)
}

/// The account's repos, unprinted (the wizard's repo picker).
pub(crate) fn account_repos(vault: &Path) -> Result<Vec<tusk_sync::AccountRepo>, CoreError> {
    let (client, _) = account_client(vault)?;
    client.list_repos().map_err(session_expired)
}

/// One-line sync summary for the human status panel (D35): `None` when
/// this vault has no cloud connection configured.
pub(crate) fn summary_line(vault: &Path) -> Option<String> {
    let config = load_config(vault).ok()?;
    let mut parts = vec![format!(
        "repo {}",
        config.repo_id.split('-').next().unwrap_or(&config.repo_id)
    )];
    if let Some((_, generation)) = load_local_rmk(vault) {
        parts.push(format!("gen {generation}"));
    }
    if let Ok(Some(state)) = crate::sync_state::load(&sync_dir(vault)) {
        parts.push(format!("{} file(s) synced", state.files.len()));
    }
    match crate::config::load(vault) {
        Ok(cfg) if cfg.sync_enabled && cfg.sync_auto => {
            parts.push(format!("auto every {}s", cfg.sync_interval_secs));
        }
        Ok(_) => parts.push("manual push/pull".to_string()),
        Err(_) => {}
    }
    Some(parts.join(" · "))
}

/// `tuskd sync status`.
pub fn status(vault: &Path) -> Result<(), CoreError> {
    use crate::style::{ACCENT, DIM, ERR, OK};
    let (cloud, config) = client(vault)?;
    anstream::println!("{DIM}server:{DIM:#}  {}", config.url);
    anstream::println!("{DIM}repo:{DIM:#}    {}", config.repo_id);
    anstream::println!("{DIM}device:{DIM:#}  {}", config.device_id);
    let key = ensure_device_key(vault)?;
    let fingerprint = tusk_sync::device_fingerprint(&key.verifying_key().to_bytes());
    if crate::style::stdout_is_tty() {
        anstream::println!(
            "{DIM}fingerprint:{DIM:#} {ACCENT}{}{ACCENT:#}",
            crate::style::fingerprint_groups(&fingerprint)
        );
    } else {
        println!("fingerprint: {fingerprint}");
    }
    let has_rmk = sync_dir(vault).join(RMK_FILE).exists();
    match cloud.fetch_wrap() {
        Ok(_) => anstream::println!(
            "{DIM}status:{DIM:#}  {OK}approved{OK:#}{}",
            if has_rmk {
                ", repo key present"
            } else {
                ", wrap available"
            }
        ),
        Err(SyncError::Http { status: 403, .. }) => {
            anstream::println!("{DIM}status:{DIM:#}  {ACCENT}pending approval{ACCENT:#}")
        }
        Err(SyncError::Http { status: 404, .. }) => anstream::println!(
            "{DIM}status:{DIM:#}  {OK}approved{OK:#}{}",
            if has_rmk {
                ", repo key present"
            } else {
                ", NO wrap and no local key"
            }
        ),
        Err(e) => anstream::println!("{DIM}status:{DIM:#}  {ERR}unreachable{ERR:#} ({e})"),
    }
    Ok(())
}

/// `tuskd sync devices`.
pub fn devices(vault: &Path) -> Result<(), CoreError> {
    let (cloud, config) = client(vault)?;
    let devices = cloud.list_devices().map_err(err)?;
    if !crate::style::stdout_is_tty() {
        for d in devices {
            println!(
                "{}  {:8}  {}  {}",
                d.device_id, d.status, d.fingerprint, d.name
            );
        }
        return Ok(());
    }
    // Aligned table (D35): status colored, fingerprints in the dashboard's
    // 4-char gold groups, ✓ marking this device.
    use crate::style::{fingerprint_groups, ACCENT, DIM, ERR, OK};
    let id_width = devices.iter().map(|d| d.device_id.len()).max().unwrap_or(2);
    let fp_width = devices
        .iter()
        .map(|d| fingerprint_groups(&d.fingerprint).len())
        .max()
        .unwrap_or(11);
    anstream::println!(
        "  {DIM}{:<id_width$}  {:<8}  {:<fp_width$}  NAME{DIM:#}",
        "DEVICE",
        "STATUS",
        "FINGERPRINT"
    );
    for d in devices {
        let mark = if d.device_id == config.device_id {
            format!("{OK}✓{OK:#}")
        } else {
            " ".to_string()
        };
        let status = match d.status.as_str() {
            "approved" => format!("{OK}{:<8}{OK:#}", d.status),
            "pending" => format!("{ACCENT}{:<8}{ACCENT:#}", d.status),
            "revoked" => format!("{ERR}{:<8}{ERR:#}", d.status),
            other => format!("{other:<8}"),
        };
        anstream::println!(
            "{mark} {:<id_width$}  {status}  {ACCENT}{:<fp_width$}{ACCENT:#}  {}",
            d.device_id,
            fingerprint_groups(&d.fingerprint),
            d.name
        );
    }
    Ok(())
}

/// `tuskd sync approve <device> --fingerprint <fp>` — the fingerprint must
/// match what the enrolling device displayed (the out-of-band check is the
/// whole security story here; there is no --force).
pub fn approve(vault: &Path, device_id: &str, fingerprint: &str) -> Result<(), CoreError> {
    let (cloud, config) = client(vault)?;
    let (cloud2, _) = client(vault)?;
    let provider = CloudProvider::new(cloud2).map_err(err)?;
    let (rmk, generation) = current_rmk(vault, &cloud, &provider, &config.repo_id)?;
    let devices = cloud.list_devices().map_err(err)?;
    let target = devices
        .iter()
        .find(|d| d.device_id == device_id)
        .ok_or_else(|| CoreError::Other(format!("no such device {device_id}")))?;
    if target.fingerprint != fingerprint {
        return Err(CoreError::Other(format!(
            "fingerprint mismatch: server lists {}, you typed {fingerprint} — do NOT approve \
             unless they match on the enrolling device's screen",
            target.fingerprint
        )));
    }
    if target.status != "pending" {
        return Err(CoreError::Other(format!(
            "device is {}, not pending",
            target.status
        )));
    }
    use base64::Engine;
    let raw: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(&target.ed25519_pubkey)
        .map_err(|e| CoreError::Other(format!("bad listed pubkey: {e}")))?
        .try_into()
        .map_err(|_| CoreError::Other("listed pubkey is not 32 bytes".into()))?;
    let pem = VerifyingKey::from_bytes(&raw)
        .map_err(|e| CoreError::Other(format!("listed pubkey invalid: {e}")))?
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| CoreError::Other(format!("pem encode: {e}")))?;
    let wrap = wrap_rmk_for_device(&rmk, &config.repo_id, &pem).map_err(err)?;
    let wrap_bytes =
        serde_json::to_vec(&wrap).map_err(|e| CoreError::Other(format!("serialize wrap: {e}")))?;
    cloud
        .approve_device(device_id, &wrap_bytes, generation)
        .map_err(err)?;
    println!("approved {device_id} ({})", target.name);
    Ok(())
}

/// The sync file set: exactly what `tuskd export` archives.
pub(crate) fn vault_files(vault: &Path) -> Result<Vec<(String, PathBuf)>, CoreError> {
    let mut out = Vec::new();
    let mut stack = vec![vault.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = match path.strip_prefix(vault) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };
            if crate::archive::skip(&rel) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                let rel = rel
                    .to_str()
                    .ok_or_else(|| CoreError::Other(format!("non-UTF8 path: {}", rel.display())))?
                    .to_string();
                out.push((rel, path));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `tuskd sync push` — upload local changes (D28: the worker's incremental
/// push path; on a never-synced vault this uploads every local file while
/// leaving remote-only files untouched).
pub fn push(vault: &Path) -> Result<(), CoreError> {
    let report = crate::sync_worker::push_only(vault)?;
    if report.pushed == 0 && report.deleted_remote == 0 {
        println!("already in sync — nothing to push");
    } else {
        println!(
            "pushed {} encrypted file(s), tombstoned {} stale blob(s)",
            report.pushed, report.deleted_remote
        );
    }
    Ok(())
}

/// `tuskd sync pull` — materialize the cloud view (additive; local files
/// not in the manifest are left alone, files in it are overwritten).
pub fn pull(vault: &Path) -> Result<(), CoreError> {
    // In-place progress on an interactive stderr (D35); silent when piped
    // so stdout stays exactly "pulled N files" either way.
    use std::io::IsTerminal;
    let progress = std::io::stderr().is_terminal();
    let written = crate::sync_worker::pull_all_with(vault, |done, total, rel| {
        if progress {
            use crate::style::DIM;
            anstream::eprint!("\r{DIM}pulling {done}/{total} {rel:<50.50}{DIM:#}");
        }
    })?;
    if progress && written > 0 {
        anstream::eprint!("\r{:<70}\r", "");
    }
    println!("pulled {written} files");
    Ok(())
}

/// `tuskd sync revoke <device>` — revoke a device and complete the
/// rotation in one command (C9/D27): server bumps the RMK generation, then
/// this device generates a new RMK, re-seals the (unchanged) key table
/// under it — no blob is renamed or re-encrypted (D22 rotation) — and
/// re-issues wraps for every remaining approved device. Prints the new
/// recovery phrase once; the old phrase and the revoked device's key are
/// dead from this point for all future data.
pub fn revoke(vault: &Path, device_id: &str) -> Result<(), CoreError> {
    let (cloud, config) = client(vault)?;
    let (cloud2, _) = client(vault)?;
    let provider = CloudProvider::new(cloud2).map_err(err)?;
    let (rmk_old, _) = current_rmk(vault, &cloud, &provider, &config.repo_id)?;

    // Open the manifest with the old key BEFORE revoking, so a failure
    // here leaves everything untouched.
    let table = match provider.get(MANIFEST) {
        Ok(sealed) => Some(KeyTable::open(&sealed, &rmk_old, &config.repo_id).map_err(err)?),
        Err(SyncError::NotFound(_)) => None,
        Err(e) => return Err(err(e)),
    };

    let generation = cloud.revoke_device(device_id).map_err(|e| match e {
        SyncError::Http { status: 409, .. } => {
            CoreError::Other("device is already revoked (or unknown)".into())
        }
        other => err(other),
    })?;

    // Rotate: new RMK, re-seal, persist locally, re-wrap the survivors.
    let rmk_new = RepoMasterKey::generate();
    if let Some(table) = table {
        provider
            .put(
                MANIFEST,
                &table.seal(&rmk_new, &config.repo_id).map_err(err)?,
            )
            .map_err(err)?;
        // Announce the manifest change on the oplog (D28) so other
        // devices' workers refresh their key promptly instead of on the
        // next content change.
        let payload = serde_json::to_vec(&crate::sync_worker::OpPayload {
            v: 1,
            put: Vec::new(),
            del: Vec::new(),
            manifest: true,
        })?;
        cloud.append_op(&payload).map_err(err)?;
    }
    store_rmk(vault, &rmk_new, generation)?;

    let mut rewrapped = 0usize;
    for device in cloud.list_devices().map_err(err)? {
        if device.status != "approved" || device.device_id == config.device_id {
            continue;
        }
        use base64::Engine;
        let raw: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(&device.ed25519_pubkey)
            .map_err(|e| CoreError::Other(format!("bad listed pubkey: {e}")))?
            .try_into()
            .map_err(|_| CoreError::Other("listed pubkey is not 32 bytes".into()))?;
        let pem = VerifyingKey::from_bytes(&raw)
            .map_err(|e| CoreError::Other(format!("listed pubkey invalid: {e}")))?
            .to_public_key_pem(LineEnding::LF)
            .map_err(|e| CoreError::Other(format!("pem encode: {e}")))?;
        let wrap = wrap_rmk_for_device(&rmk_new, &config.repo_id, &pem).map_err(err)?;
        let wrap_bytes = serde_json::to_vec(&wrap)
            .map_err(|e| CoreError::Other(format!("serialize wrap: {e}")))?;
        cloud
            .push_wrap(&device.device_id, &wrap_bytes, generation)
            .map_err(err)?;
        rewrapped += 1;
    }

    println!("revoked {device_id}; rotated to generation {generation}");
    println!("re-wrapped the repo key for {rewrapped} remaining device(s)");
    println!();
    print_recovery_phrase(&rmk_new.to_mnemonic().map_err(err)?, true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Visible width of a styled line: chars with SGR escapes stripped.
    fn visible_width(line: &str) -> usize {
        let mut width = 0usize;
        let mut in_escape = false;
        for c in line.chars() {
            if in_escape {
                if c == 'm' {
                    in_escape = false;
                }
            } else if c == '\u{1b}' {
                in_escape = true;
            } else {
                width += 1;
            }
        }
        width
    }

    #[test]
    fn recovery_box_lines_align_and_words_copy_clean() {
        let phrase = "abandon ability able about above absent absorb abstract absurd abuse \
                      access accident account accuse achieve acid acoustic acquire across act \
                      action actor actress actual";
        for rotated in [false, true] {
            let lines = recovery_phrase_box(phrase, rotated);
            let widths: Vec<usize> = lines.iter().map(|l| visible_width(l)).collect();
            assert!(
                widths.iter().all(|w| *w == widths[0]),
                "ragged box: {widths:?}"
            );
            // Every phrase word appears, unstyled: the char right before a
            // word is a plain space, never an escape terminator.
            let all = lines.join("\n");
            for word in phrase.split_whitespace() {
                let at = all.find(&format!(" {word}")).expect(word);
                assert!(!all[..at].ends_with('\u{1b}'));
            }
        }
    }

    #[test]
    fn junk_basenames_are_not_names() {
        for junk in [
            "a", "ab", "..", "-", "tmp", "TMP", "test", "untitled", "new",
        ] {
            assert!(!looks_like_a_name(junk), "{junk:?} accepted");
        }
        for good in ["notes", "my-vault", "work2026", "hq-docs"] {
            assert!(looks_like_a_name(good), "{good:?} rejected");
        }
    }

    #[test]
    fn derive_prefers_basename_then_parent_then_vault() {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        let single = projects.join("a");
        std::fs::create_dir_all(&single).unwrap();
        // A junk basename picks up its parent for signal.
        assert_eq!(derive_repo_name(&single), "projects-a");
        // A decent basename wins outright.
        assert_eq!(derive_repo_name(&projects), "projects");
        // Junk all the way up falls back to "vault".
        let deep = root.path().join("x").join("y");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(derive_repo_name(&deep), "vault");
        // A path that doesn't resolve falls back too.
        assert_eq!(derive_repo_name(Path::new("/no/such/path/here")), "vault");
    }

    #[test]
    fn home_directory_is_never_the_default_name() {
        if let Some(home) = std::env::var_os("HOME") {
            if Path::new(&home).canonicalize().is_ok() {
                assert_eq!(derive_repo_name(Path::new(&home)), "vault");
            }
        }
    }
}

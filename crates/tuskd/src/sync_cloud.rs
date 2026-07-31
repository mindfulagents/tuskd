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
    println!("next: tuskd sync init   # create a cloud repo for this vault");
    Ok(())
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
        None => vault
            .canonicalize()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "vault".to_string()),
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
    println!("RECOVERY PHRASE — write this down; it is shown exactly once:");
    println!("  {}", rmk.to_mnemonic().map_err(err)?);
    println!();
    println!("next: tuskd sync push   # or just run the daemon — it syncs automatically");
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
    for repo in repos {
        println!(
            "{}  gen {}  {}",
            repo.repo_id, repo.rmk_generation, repo.name
        );
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

/// `tuskd sync connect` — enroll this vault's device into an existing repo.
pub fn connect(
    vault: &Path,
    url: &str,
    repo_id: &str,
    name: Option<String>,
    phrase: Option<String>,
) -> Result<(), CoreError> {
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
    println!("fingerprint: {fingerprint}");
    println!(
        "on an approved device, run: tuskd sync approve {device_id} --fingerprint {fingerprint}"
    );
    Ok(())
}

/// `tuskd sync status`.
pub fn status(vault: &Path) -> Result<(), CoreError> {
    let (cloud, config) = client(vault)?;
    println!("server:  {}", config.url);
    println!("repo:    {}", config.repo_id);
    println!("device:  {}", config.device_id);
    let key = ensure_device_key(vault)?;
    println!(
        "fingerprint: {}",
        tusk_sync::device_fingerprint(&key.verifying_key().to_bytes())
    );
    let has_rmk = sync_dir(vault).join(RMK_FILE).exists();
    match cloud.fetch_wrap() {
        Ok(_) => println!(
            "status:  approved{}",
            if has_rmk {
                ", repo key present"
            } else {
                ", wrap available"
            }
        ),
        Err(SyncError::Http { status: 403, .. }) => println!("status:  pending approval"),
        Err(SyncError::Http { status: 404, .. }) => println!(
            "status:  approved{}",
            if has_rmk {
                ", repo key present"
            } else {
                ", NO wrap and no local key"
            }
        ),
        Err(e) => println!("status:  unreachable ({e})"),
    }
    Ok(())
}

/// `tuskd sync devices`.
pub fn devices(vault: &Path) -> Result<(), CoreError> {
    let (cloud, _) = client(vault)?;
    let devices = cloud.list_devices().map_err(err)?;
    for d in devices {
        println!(
            "{}  {:8}  {}  {}",
            d.device_id, d.status, d.fingerprint, d.name
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
    let written = crate::sync_worker::pull_all(vault)?;
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
    println!("NEW RECOVERY PHRASE — the old one is now useless; write this down:");
    println!("  {}", rmk_new.to_mnemonic().map_err(err)?);
    Ok(())
}

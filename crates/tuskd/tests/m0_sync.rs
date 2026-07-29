//! M0 sync groundwork — tuskd wiring tests (D21): config flag off by
//! default, identity persistence across CoreHost opens, 0600 device key,
//! reconciliation on reopen, export exclusion. Temp-dir vaults only; no
//! daemon, no ports.

use std::path::{Path, PathBuf};
use tuskd::config;
use tuskd::runtime::CoreHost;

fn write_config(vault: &Path, body: &str) {
    let tusk = vault.join(".tusk");
    std::fs::create_dir_all(&tusk).unwrap();
    std::fs::write(tusk.join("tuskd.toml"), body).unwrap();
}

fn open(vault: &Path) -> CoreHost {
    let cfg = config::load(vault).unwrap();
    CoreHost::open(&cfg, false).unwrap()
}

fn sync_dir(vault: &Path) -> PathBuf {
    // CoreHost canonicalizes the root; do the same for comparisons.
    std::fs::canonicalize(vault).unwrap().join(".tusk/sync")
}

fn sample_record(host: &CoreHost, body: &str) -> tusk_core::record::Record {
    host.ctx.vault.new_record(
        tusk_core::record::RecordType::Semantic,
        tusk_core::scope::Scope::parse("project:opentusk").unwrap(),
        "test-agent",
        body,
    )
}

#[test]
fn sync_disabled_by_default_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // No config at all, and the shipped default config: both must stay off.
    let cfg = config::load(dir.path()).unwrap();
    assert!(!cfg.sync_enabled);
    write_config(dir.path(), config::DEFAULT_TOML);
    let cfg = config::load(dir.path()).unwrap();
    assert!(!cfg.sync_enabled);

    let host = open(dir.path());
    let rec = sample_record(&host, "no sync side effects");
    host.ctx.vault.write(&rec).unwrap();
    assert!(host.ctx.vault.journal().is_none());
    host.shutdown();
    assert!(!sync_dir(dir.path()).exists());
}

#[test]
fn identities_persist_across_opens_and_key_is_private() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), "[sync]\nenabled = true\n");

    let host = open(dir.path());
    host.shutdown();
    let sync = sync_dir(dir.path());
    let vault_id = std::fs::read_to_string(sync.join("vault_id")).unwrap();
    let device_pem = std::fs::read_to_string(sync.join("device.pem")).unwrap();
    assert!(device_pem.contains("PRIVATE KEY"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let key_mode = std::fs::metadata(sync.join("device.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(key_mode & 0o777, 0o600, "device.pem must be 0600");
        let dir_mode = std::fs::metadata(&sync).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700, "sync dir must be 0700");
    }

    // Restart: same identity, same key, journal still verifies.
    let host = open(dir.path());
    host.shutdown();
    assert_eq!(
        std::fs::read_to_string(sync.join("vault_id")).unwrap(),
        vault_id
    );
    assert_eq!(
        std::fs::read_to_string(sync.join("device.pem")).unwrap(),
        device_pem
    );
}

#[test]
fn offline_edits_are_journaled_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), "[sync]\nenabled = true\n");

    let host = open(dir.path());
    let kept = sample_record(&host, "kept record");
    let doomed = sample_record(&host, "doomed record");
    let kept_path = host.ctx.vault.write(&kept).unwrap();
    let doomed_path = host.ctx.vault.write(&doomed).unwrap();
    let baseline = host.ctx.vault.journal().unwrap().entries().unwrap().len();
    host.shutdown();

    // Daemon down: edit one record, delete the other.
    let mut text = std::fs::read_to_string(&kept_path).unwrap();
    text.push_str("\noffline edit\n");
    std::fs::write(&kept_path, text).unwrap();
    std::fs::remove_file(&doomed_path).unwrap();

    let host = open(dir.path());
    let journal = host.ctx.vault.journal().unwrap();
    let entries = journal.entries().unwrap();
    let tail = &entries[baseline..];
    let kept_rel = host.ctx.vault.rel_path(&kept_path).unwrap();
    let doomed_rel = host.ctx.vault.rel_path(&doomed_path).unwrap();
    use tusk_core::sync::OpKind;
    assert!(tail
        .iter()
        .any(|e| e.kind == OpKind::Put && e.path == kept_rel));
    assert!(tail
        .iter()
        .any(|e| e.kind == OpKind::Tombstone && e.path == doomed_rel));
    host.shutdown();
}

#[test]
fn export_never_ships_sync_state() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), "[sync]\nenabled = true\n");
    let host = open(dir.path());
    let rec = sample_record(&host, "exported record");
    host.ctx.vault.write(&rec).unwrap();
    host.shutdown();
    assert!(sync_dir(dir.path()).join("device.pem").exists());

    let archive = dir.path().join("out.tar.gz");
    tuskd::archive::export(dir.path(), &archive).unwrap();
    let file = std::fs::File::open(&archive).unwrap();
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let names: Vec<String> = tar
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().display().to_string())
        .collect();
    assert!(names.iter().any(|n| n.starts_with("memory/")));
    assert!(
        !names.iter().any(|n| n.starts_with(".tusk/sync")),
        "sync state leaked into export: {names:?}"
    );
}

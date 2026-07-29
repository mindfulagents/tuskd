//! M0 sync groundwork exit tests (HOT_CACHE_SYNC_PROPOSAL §6 items 1–2, D21):
//! journal append + hash chain, tombstone on forget, reconciliation after
//! offline edits, identity persistence. Temp-dir scratch vaults only.

use chrono::{Duration, TimeZone, Utc};
use std::path::Path;
use std::sync::Arc;
use tusk_core::clock::{Clock, FakeClock};
use tusk_core::record::{Record, RecordType};
use tusk_core::scope::Scope;
use tusk_core::sync::{self, Journal, OpKind};
use tusk_core::vault::VaultStore;

fn fake_clock() -> FakeClock {
    FakeClock::new(Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, 0).unwrap())
}

/// Vault with sync enabled the way tuskd does it: sync dir + vault_id +
/// journal attached. Returns (vault, clock, vault_id).
fn synced_vault(dir: &Path) -> (VaultStore, FakeClock, String) {
    let clock = fake_clock();
    let vs = VaultStore::init(dir, Arc::new(clock.clone())).unwrap();
    let sync_dir = vs.tusk_dir().join("sync");
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let vault_id = sync::load_or_create_vault_id(&sync_dir, &clock_dyn).unwrap();
    let journal = Journal::open(
        &sync_dir.join(sync::JOURNAL_FILE),
        &vault_id,
        Arc::new(clock.clone()),
    )
    .unwrap();
    vs.attach_journal(Arc::new(journal));
    (vs, clock, vault_id)
}

fn sample(vs: &VaultStore, body: &str) -> Record {
    vs.new_record(
        RecordType::Semantic,
        Scope::parse("project:opentusk").unwrap(),
        "hermes-dev",
        body,
    )
}

fn journal_path(vs: &VaultStore) -> std::path::PathBuf {
    vs.tusk_dir().join("sync").join(sync::JOURNAL_FILE)
}

#[test]
fn every_mutation_appends_and_the_chain_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, clock, vault_id) = synced_vault(dir.path());

    let rec = sample(&vs, "First fact.");
    vs.write(&rec).unwrap();
    clock.advance(Duration::seconds(1));
    vs.update_telemetry(&rec.id, 1, 1.0, clock.now()).unwrap();
    clock.advance(Duration::seconds(1));
    let mut newer = sample(&vs, "Corrected fact.");
    vs.supersede(&mut newer, &rec.id).unwrap();
    clock.advance(Duration::seconds(1));
    vs.forget(&newer.id).unwrap();

    let journal = vs.journal().unwrap();
    let entries = journal.entries().unwrap();
    let kinds: Vec<OpKind> = entries.iter().map(|e| e.kind).collect();
    // write=put, telemetry=patch, supersede = put(new)+patch(old invalidate),
    // forget=tombstone.
    assert_eq!(
        kinds,
        vec![
            OpKind::Put,
            OpKind::Patch,
            OpKind::Put,
            OpKind::Patch,
            OpKind::Tombstone
        ]
    );
    // Every entry carries ULID op_id, ts, path; non-tombstones carry a hash.
    for e in &entries {
        ulid::Ulid::from_string(&e.op_id).unwrap();
        assert!(e.path.starts_with("memory/project/opentusk/"));
        assert!(!e.ts.is_empty());
        match e.kind {
            OpKind::Tombstone => assert!(e.content_hash.is_none()),
            _ => assert!(e.content_hash.is_some()),
        }
    }
    // Chain links: prev of entry N = sha256 of line N-1; genesis bound to
    // vault_id. Verified end-to-end by reopening.
    assert_eq!(entries[0].prev, sync::genesis_hash(&vault_id));
    let raw = std::fs::read_to_string(journal_path(&vs)).unwrap();
    let lines: Vec<&str> = raw.lines().collect();
    for (i, e) in entries.iter().enumerate().skip(1) {
        assert_eq!(e.prev, sync::content_hash(lines[i - 1].as_bytes()));
    }
    Journal::open(&journal_path(&vs), &vault_id, Arc::new(clock.clone())).unwrap();
    // Content hash matches the bytes the mutation left on disk.
    let (path, _) = vs.get(&rec.id).unwrap();
    let on_disk = sync::content_hash(&std::fs::read(&path).unwrap());
    assert_eq!(entries[3].content_hash.as_deref(), Some(on_disk.as_str()));
}

#[test]
fn forget_leaves_a_tombstone_not_silence() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, _, _) = synced_vault(dir.path());
    let rec = sample(&vs, "ephemeral");
    let path = vs.write(&rec).unwrap();
    let rel = vs.rel_path(&path).unwrap();
    vs.forget(&rec.id).unwrap();
    assert!(!path.exists());

    let journal = vs.journal().unwrap();
    let last = journal.entries().unwrap().pop().unwrap();
    assert_eq!(last.kind, OpKind::Tombstone);
    assert_eq!(last.path, rel);
    assert!(last.content_hash.is_none());
    // And the folded live state no longer contains the path.
    assert!(!journal.live_state().unwrap().contains_key(&rel));
}

#[test]
fn tampering_breaks_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, clock, vault_id) = synced_vault(dir.path());
    vs.write(&sample(&vs, "one")).unwrap();
    vs.write(&sample(&vs, "two")).unwrap();

    let path = journal_path(&vs);
    let mut raw = std::fs::read_to_string(&path).unwrap();
    // Flip a hash character inside the FIRST line (append-only violated).
    let idx = raw.find("\"content_hash\":\"").unwrap() + "\"content_hash\":\"".len();
    let orig = raw.as_bytes()[idx];
    let flipped = if orig == b'0' { '1' } else { '0' };
    raw.replace_range(idx..idx + 1, &flipped.to_string());
    std::fs::write(&path, raw).unwrap();

    let err = Journal::open(&path, &vault_id, Arc::new(clock.clone())).unwrap_err();
    assert!(err.to_string().contains("hash chain broken"), "{err}");

    // Wrong vault_id also fails from line one (journal is identity-bound).
    let dir2 = tempfile::tempdir().unwrap();
    let (vs2, _, _) = synced_vault(dir2.path());
    vs2.write(&sample(&vs2, "x")).unwrap();
    let err = Journal::open(
        &journal_path(&vs2),
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        Arc::new(clock),
    )
    .unwrap_err();
    assert!(err.to_string().contains("hash chain broken"), "{err}");
}

#[test]
fn torn_final_append_self_heals() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, clock, vault_id) = synced_vault(dir.path());
    vs.write(&sample(&vs, "kept")).unwrap();

    // Simulate a crash mid-append: partial JSON, no trailing newline.
    let path = journal_path(&vs);
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"{\"op_id\":\"01TORN").unwrap();
    drop(f);

    let journal = Journal::open(&path, &vault_id, Arc::new(clock.clone())).unwrap();
    assert_eq!(journal.entries().unwrap().len(), 1); // torn line dropped
                                                     // The healed journal accepts appends and the chain still verifies.
    journal
        .append(OpKind::Put, "memory/user/x.md", Some("ab"))
        .unwrap();
    let reopened = Journal::open(&path, &vault_id, Arc::new(clock)).unwrap();
    assert_eq!(reopened.entries().unwrap().len(), 2);
}

#[test]
fn reconciliation_journals_offline_edits() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, clock, vault_id) = synced_vault(dir.path());
    let edited = sample(&vs, "will be edited offline");
    let removed = sample(&vs, "will be removed offline");
    let untouched = sample(&vs, "untouched");
    let edited_path = vs.write(&edited).unwrap();
    let removed_path = vs.write(&removed).unwrap();
    vs.write(&untouched).unwrap();
    let baseline = vs.journal().unwrap().entries().unwrap().len();

    // "Daemon down": mutate files directly, then reopen the vault.
    let mut on_disk = std::fs::read_to_string(&edited_path).unwrap();
    on_disk.push_str("\nedited while the daemon was down\n");
    std::fs::write(&edited_path, &on_disk).unwrap();
    std::fs::remove_file(&removed_path).unwrap();
    let new_file = vs.memory_dir().join("user").join("01NEWOFFLINERECORD.md");
    std::fs::create_dir_all(new_file.parent().unwrap()).unwrap();
    std::fs::write(&new_file, "# not even frontmatter\n").unwrap();

    let vs2 = VaultStore::init(dir.path(), Arc::new(clock.clone())).unwrap();
    let journal = Journal::open(&journal_path(&vs2), &vault_id, Arc::new(clock.clone())).unwrap();
    vs2.attach_journal(Arc::new(journal));
    let appended = sync::reconcile(&vs2).unwrap();
    assert_eq!(appended, 3, "edited put + new put + removed tombstone");

    let entries = vs2.journal().unwrap().entries().unwrap();
    let tail = &entries[baseline..];
    let edited_rel = vs2.rel_path(&edited_path).unwrap();
    let removed_rel = vs2.rel_path(&removed_path).unwrap();
    let new_rel = vs2.rel_path(&new_file).unwrap();
    assert!(tail.iter().any(|e| e.kind == OpKind::Put
        && e.path == edited_rel
        && e.content_hash.as_deref() == Some(sync::content_hash(on_disk.as_bytes()).as_str())));
    assert!(tail
        .iter()
        .any(|e| e.kind == OpKind::Put && e.path == new_rel));
    assert!(tail
        .iter()
        .any(|e| e.kind == OpKind::Tombstone && e.path == removed_rel));

    // Idempotent: a second scan with nothing changed appends nothing.
    assert_eq!(sync::reconcile(&vs2).unwrap(), 0);
}

#[test]
fn vault_id_is_stable_across_opens() {
    let dir = tempfile::tempdir().unwrap();
    let sync_dir = dir.path().join(".tusk").join("sync");
    let clock: Arc<dyn Clock> = Arc::new(fake_clock());
    let first = sync::load_or_create_vault_id(&sync_dir, &clock).unwrap();
    ulid::Ulid::from_string(&first).unwrap();
    let later: Arc<dyn Clock> = Arc::new(FakeClock::new(
        Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap(),
    ));
    let second = sync::load_or_create_vault_id(&sync_dir, &later).unwrap();
    assert_eq!(first, second);
    // Corrupt id is rejected, not silently regenerated.
    std::fs::write(sync_dir.join(sync::VAULT_ID_FILE), "not-a-ulid\n").unwrap();
    assert!(sync::load_or_create_vault_id(&sync_dir, &clock).is_err());
}

#[test]
fn device_key_roundtrips_through_pem() {
    let (private_pem, public_pem) = sync::generate_device_key().unwrap();
    assert!(private_pem.contains("PRIVATE KEY"));
    assert_eq!(
        sync::device_public_key_pem(&private_pem).unwrap(),
        public_pem
    );
    assert!(sync::device_public_key_pem("garbage").is_err());
}

#[test]
fn no_journal_attached_means_no_sync_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let clock = fake_clock();
    let vs = VaultStore::init(dir.path(), Arc::new(clock)).unwrap();
    let rec = sample(&vs, "plain vault");
    vs.write(&rec).unwrap();
    vs.forget(&rec.id).unwrap();
    assert!(vs.journal().is_none());
    assert_eq!(sync::reconcile(&vs).unwrap(), 0);
    assert!(!vs.tusk_dir().join("sync").exists());
}

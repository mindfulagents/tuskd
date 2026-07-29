//! M0 exit test: full two-device round-trip through a `StorageProvider`.
//!
//! Device A encrypts a scratch vault and pushes it through `LocalProvider`;
//! a "second device" (fresh directory, no shared state) recovers the RMK —
//! once from the 24-word Secret Key phrase, once from a device wrap — pulls,
//! decrypts, and materializes byte-identical content. Plus the
//! server-blindness audit: nothing the provider stores (names or bytes)
//! contains any plaintext substring of the source.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use tusk_sync::crypto::{KeyTable, RepoMasterKey};
use tusk_sync::wrap::{unwrap_rmk_for_device, wrap_rmk_for_device};
use tusk_sync::{LocalProvider, StorageProvider};

const REPO_ID: &str = "repo-01JZX7Y9QK5ROUNDTRIP";
const MANIFEST: &str = "manifest";

/// Distinctive markers that must never appear server-side.
const SECRETS: &[&str] = &[
    "TOP-SECRET-ALPHA-launch-plan",
    "TOP-SECRET-BETA-personnel-note",
    "TOP-SECRET-GAMMA-unicode-\u{1F418}-body",
];

fn scratch_vault(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let files: &[(&str, Vec<u8>)] = &[
        (
            "memory/team/alpha.md",
            format!("---\nscope: team\n---\n{}\n", SECRETS[0]).into_bytes(),
        ),
        (
            "memory/personal/beta.md",
            format!("---\nscope: personal\n---\n{}\n", SECRETS[1]).into_bytes(),
        ),
        (
            "skills/gamma.md",
            format!("# skill\n{}\n", SECRETS[2]).into_bytes(),
        ),
        // A binary-ish object with every byte value, to keep the pipeline
        // honest about non-UTF8 content.
        (".tusk/queue/review.json", (0u8..=255).collect()),
    ];
    let mut out = BTreeMap::new();
    for (rel, bytes) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        out.insert(rel.to_string(), bytes.clone());
    }
    out
}

/// Device A: encrypt every object, push blobs + sealed manifest.
fn push_all(
    provider: &dyn StorageProvider,
    rmk: &RepoMasterKey,
    objects: &BTreeMap<String, Vec<u8>>,
) -> KeyTable {
    let mut table = KeyTable::new();
    for (rel, plaintext) in objects {
        let entry = table.entry(rmk, rel).unwrap();
        let blob = entry
            .dek()
            .unwrap()
            .encrypt(REPO_ID, rel, plaintext)
            .unwrap();
        provider.put(&entry.blob, &blob).unwrap();
    }
    provider
        .put(MANIFEST, &table.seal(rmk, REPO_ID).unwrap())
        .unwrap();
    table
}

/// Second device: given only the provider and a recovered RMK, materialize
/// the vault into `dest` and return rel_path → bytes.
fn pull_all(
    provider: &dyn StorageProvider,
    rmk: &RepoMasterKey,
    dest: &Path,
) -> BTreeMap<String, Vec<u8>> {
    let sealed = provider.get(MANIFEST).unwrap();
    let table = KeyTable::open(&sealed, rmk, REPO_ID).unwrap();
    let mut out = BTreeMap::new();
    for (rel, entry) in table.iter() {
        let blob = provider.get(&entry.blob).unwrap();
        let plaintext = entry.dek().unwrap().decrypt(REPO_ID, rel, &blob).unwrap();
        let path = dest.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &plaintext).unwrap();
        out.insert(rel.clone(), plaintext);
    }
    out
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn two_device_round_trip_with_server_blindness() {
    let tmp = tempfile::tempdir().unwrap();
    let vault_a = tmp.path().join("device-a/vault");
    let store_dir = tmp.path().join("server/blobs");

    // --- Device A: scratch vault, encrypt, push. -------------------------
    let originals = scratch_vault(&vault_a);
    let rmk = RepoMasterKey::generate();
    let phrase = rmk.to_mnemonic().unwrap();
    let provider = LocalProvider::new(&store_dir).unwrap();
    push_all(&provider, &rmk, &originals);

    // Device A also approves a future device B: wrap the RMK to B's key.
    let (b_private_pem, b_public_pem) = tusk_core::sync::generate_device_key().unwrap();
    let wrap = wrap_rmk_for_device(&rmk, REPO_ID, &b_public_pem).unwrap();
    let wrap_json = serde_json::to_string(&wrap).unwrap(); // travels via server

    // --- Server-blindness audit over everything at rest. ------------------
    let names = provider.list().unwrap();
    assert_eq!(names.len(), originals.len() + 1); // objects + manifest
    for name in &names {
        if name != MANIFEST {
            assert_eq!(name.len(), 64);
            assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        }
        // Names leak no structure.
        for rel in originals.keys() {
            for part in rel.split(['/', '.']).filter(|p| !p.is_empty()) {
                assert!(!name.contains(part), "blob name {name} leaks {part:?}");
            }
        }
        // Stored bytes contain no plaintext: not the secrets, not the paths,
        // not any 16-byte window of any source object.
        let bytes = provider.get(name).unwrap();
        for secret in SECRETS {
            assert!(!contains_subslice(&bytes, secret.as_bytes()));
        }
        for (rel, plain) in &originals {
            assert!(!contains_subslice(&bytes, rel.as_bytes()));
            if plain.len() >= 16 {
                for window in plain.windows(16).step_by(8) {
                    assert!(
                        !contains_subslice(&bytes, window),
                        "stored {name} leaks source bytes"
                    );
                }
            }
        }
    }

    // --- Device B path 1: recover from the Secret Key phrase. -------------
    let rmk_from_phrase = RepoMasterKey::from_mnemonic(&phrase).unwrap();
    let dest1 = tmp.path().join("device-b-phrase/vault");
    let pulled1 = pull_all(&provider, &rmk_from_phrase, &dest1);
    assert_eq!(pulled1, originals, "phrase-recovered vault differs");
    for (rel, plain) in &originals {
        assert_eq!(&fs::read(dest1.join(rel)).unwrap(), plain);
    }

    // --- Device B path 2: recover via the device wrap. ---------------------
    let wrap: tusk_sync::DeviceWrap = serde_json::from_str(&wrap_json).unwrap();
    let rmk_from_wrap = unwrap_rmk_for_device(&wrap, REPO_ID, &b_private_pem).unwrap();
    let dest2 = tmp.path().join("device-b-wrap/vault");
    let pulled2 = pull_all(&provider, &rmk_from_wrap, &dest2);
    assert_eq!(pulled2, originals, "wrap-recovered vault differs");
}

#[test]
fn local_provider_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(&tmp.path().join("blobs")).unwrap();

    assert_eq!(provider.list().unwrap(), Vec::<String>::new());
    provider.put("aa11", b"one").unwrap();
    provider.put("bb22", b"two").unwrap();
    assert_eq!(
        provider.list().unwrap(),
        vec!["aa11".to_string(), "bb22".to_string()]
    );
    assert_eq!(provider.get("aa11").unwrap(), b"one");

    // Overwrite.
    provider.put("aa11", b"one-v2").unwrap();
    assert_eq!(provider.get("aa11").unwrap(), b"one-v2");

    // Missing get errors; delete is idempotent.
    assert!(matches!(
        provider.get("cc33"),
        Err(tusk_sync::SyncError::NotFound(_))
    ));
    provider.delete("bb22").unwrap();
    provider.delete("bb22").unwrap();
    assert_eq!(provider.list().unwrap(), vec!["aa11".to_string()]);

    // Path-shaped and hidden names are rejected outright.
    for bad in [
        "../escape",
        "a/b",
        "",
        ".hidden",
        "UPPER",
        "name with space",
    ] {
        assert!(
            matches!(
                provider.put(bad, b"x"),
                Err(tusk_sync::SyncError::InvalidName(_))
            ),
            "accepted {bad:?}"
        );
        assert!(provider.get(bad).is_err());
    }
}

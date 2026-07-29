//! Crypto-layer exit tests (M0, D22): Secret Key encodings, DEK/AEAD
//! behavior (tamper + wrong-AAD), key-table sealing, cheap rotation, device
//! wraps, blob naming.

use tusk_sync::crypto::{KeyTable, RepoMasterKey};
use tusk_sync::wrap::{unwrap_rmk_for_device, wrap_rmk_for_device};

const REPO_ID: &str = "repo-01JZX7Y9QK5TESTREPO";

// ---------------------------------------------------------------------------
// Repo Secret Key: 24-word phrase + otsk_ compact form
// ---------------------------------------------------------------------------

#[test]
fn mnemonic_is_24_words_and_round_trips() {
    let rmk = RepoMasterKey::generate();
    let phrase = rmk.to_mnemonic().unwrap();
    assert_eq!(phrase.split_whitespace().count(), 24);
    let recovered = RepoMasterKey::from_mnemonic(&phrase).unwrap();
    assert_eq!(rmk, recovered);
}

#[test]
fn mnemonic_parse_tolerates_case_and_whitespace() {
    let rmk = RepoMasterKey::generate();
    let phrase = rmk.to_mnemonic().unwrap();
    let messy = format!("  {}  ", phrase.to_uppercase().replace(' ', "\n  "));
    assert_eq!(RepoMasterKey::from_mnemonic(&messy).unwrap(), rmk);
}

#[test]
fn mnemonic_typo_detected() {
    let rmk = RepoMasterKey::generate();
    let phrase = rmk.to_mnemonic().unwrap();
    let mut words: Vec<&str> = phrase.split_whitespace().collect();
    // Swap one word for a different valid wordlist word: the BIP39 checksum
    // must catch it (or, rarely, the word equals the original — pick the
    // other candidate then).
    let replacement = if words[3] == "abandon" {
        "zoo"
    } else {
        "abandon"
    };
    words[3] = replacement;
    assert!(RepoMasterKey::from_mnemonic(&words.join(" ")).is_err());
    // A non-wordlist word must fail too.
    words[3] = "opentusk";
    assert!(RepoMasterKey::from_mnemonic(&words.join(" ")).is_err());
}

#[test]
fn otsk_round_trips_and_detects_typos() {
    let rmk = RepoMasterKey::generate();
    let otsk = rmk.to_otsk();
    assert!(otsk.starts_with("otsk_"));
    assert_eq!(otsk.len(), 5 + 64 + 8);
    assert_eq!(RepoMasterKey::from_otsk(&otsk).unwrap(), rmk);

    // Flip one hex digit in the key body: checksum mismatch.
    let mut chars: Vec<char> = otsk.chars().collect();
    chars[10] = if chars[10] == 'a' { 'b' } else { 'a' };
    let typo: String = chars.iter().collect();
    assert!(RepoMasterKey::from_otsk(&typo).is_err());

    // Wrong prefix and truncation fail.
    assert!(RepoMasterKey::from_otsk(&otsk[1..]).is_err());
    assert!(RepoMasterKey::from_otsk(&otsk[..otsk.len() - 2]).is_err());
}

// ---------------------------------------------------------------------------
// Object encryption: tamper + wrong AAD
// ---------------------------------------------------------------------------

#[test]
fn object_tamper_detected() {
    let mut table = KeyTable::new();
    let rmk = RepoMasterKey::generate();
    let entry = table.entry(&rmk, "memory/team/alpha.md").unwrap();
    let dek = entry.dek().unwrap();
    let blob = dek
        .encrypt(REPO_ID, "memory/team/alpha.md", b"plaintext body")
        .unwrap();

    // Flip one ciphertext byte (past the 24-byte nonce): decryption fails.
    let mut tampered = blob.clone();
    tampered[30] ^= 0x01;
    assert!(dek
        .decrypt(REPO_ID, "memory/team/alpha.md", &tampered)
        .is_err());

    // Flip a nonce byte: also fails (tag covers the nonce-derived stream).
    let mut nonce_tampered = blob.clone();
    nonce_tampered[0] ^= 0x01;
    assert!(dek
        .decrypt(REPO_ID, "memory/team/alpha.md", &nonce_tampered)
        .is_err());

    // Untampered still opens.
    assert_eq!(
        dek.decrypt(REPO_ID, "memory/team/alpha.md", &blob).unwrap(),
        b"plaintext body"
    );
}

#[test]
fn object_wrong_aad_fails() {
    let mut table = KeyTable::new();
    let rmk = RepoMasterKey::generate();
    let entry = table.entry(&rmk, "memory/a.md").unwrap();
    let dek = entry.dek().unwrap();
    let blob = dek
        .encrypt(REPO_ID, "memory/a.md", b"bound to repo and path")
        .unwrap();

    // Same DEK, wrong path: the blob must not decrypt (no cross-path splicing).
    assert!(dek.decrypt(REPO_ID, "memory/b.md", &blob).is_err());
    // Same DEK, wrong repo: same.
    assert!(dek.decrypt("repo-OTHER", "memory/a.md", &blob).is_err());
}

// ---------------------------------------------------------------------------
// Key table sealing + cheap rotation
// ---------------------------------------------------------------------------

#[test]
fn key_table_seal_open_round_trip_and_tamper() {
    let rmk = RepoMasterKey::generate();
    let mut table = KeyTable::new();
    table.entry(&rmk, "memory/a.md").unwrap();
    table.entry(&rmk, "skills/how-to.md").unwrap();

    let sealed = table.seal(&rmk, REPO_ID).unwrap();
    assert_eq!(KeyTable::open(&sealed, &rmk, REPO_ID).unwrap(), table);

    // Wrong RMK cannot open it.
    assert!(KeyTable::open(&sealed, &RepoMasterKey::generate(), REPO_ID).is_err());
    // Wrong repo (AAD) cannot open it.
    assert!(KeyTable::open(&sealed, &rmk, "repo-OTHER").is_err());
    // Tampered bytes cannot open it.
    let mut tampered = sealed.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    assert!(KeyTable::open(&tampered, &rmk, REPO_ID).is_err());
}

#[test]
fn rotation_reencrypts_only_the_key_table() {
    let rmk_old = RepoMasterKey::generate();
    let mut table = KeyTable::new();

    // Encrypt two blobs under the old RMK's table.
    let paths = ["memory/team/alpha.md", "memory/personal/beta.md"];
    let mut blobs = Vec::new();
    for path in paths {
        let entry = table.entry(&rmk_old, path).unwrap();
        let blob = entry
            .dek()
            .unwrap()
            .encrypt(REPO_ID, path, format!("content of {path}").as_bytes())
            .unwrap();
        blobs.push((path, entry.blob.clone(), blob));
    }

    // Rotate: re-seal the (unchanged) table under a new RMK. This is the
    // whole rotation — no blob is touched.
    let rmk_new = RepoMasterKey::generate();
    let resealed = table.seal(&rmk_new, REPO_ID).unwrap();

    // Old RMK can no longer open the rotated table.
    assert!(KeyTable::open(&resealed, &rmk_old, REPO_ID).is_err());

    // New RMK opens it, and every pre-rotation blob decrypts *unchanged*:
    // same stored name, same ciphertext bytes, no re-encryption needed.
    let recovered = KeyTable::open(&resealed, &rmk_new, REPO_ID).unwrap();
    for (path, name, blob) in &blobs {
        let entry = recovered.get(path).unwrap();
        assert_eq!(&entry.blob, name, "rotation must not rename blobs");
        let plain = entry.dek().unwrap().decrypt(REPO_ID, path, blob).unwrap();
        assert_eq!(plain, format!("content of {path}").into_bytes());
    }

    // New objects after rotation get names under the new RMK.
    let mut recovered = recovered;
    let fresh = recovered.entry(&rmk_new, "memory/gamma.md").unwrap();
    assert_eq!(fresh.blob, rmk_new.blob_name("memory/gamma.md").unwrap());
}

// ---------------------------------------------------------------------------
// Device wraps (X25519 from the slice-1 ed25519 identity)
// ---------------------------------------------------------------------------

#[test]
fn device_wrap_round_trip() {
    let rmk = RepoMasterKey::generate();
    let (private_pem, public_pem) = tusk_core::sync::generate_device_key().unwrap();

    let wrap = wrap_rmk_for_device(&rmk, REPO_ID, &public_pem).unwrap();
    assert_eq!(wrap.v, 1);

    // The wrap survives serialization (it travels through the server).
    let json = serde_json::to_string(&wrap).unwrap();
    let wrap: tusk_sync::DeviceWrap = serde_json::from_str(&json).unwrap();

    let recovered = unwrap_rmk_for_device(&wrap, REPO_ID, &private_pem).unwrap();
    assert_eq!(recovered, rmk);
}

#[test]
fn device_wrap_wrong_device_or_repo_fails() {
    let rmk = RepoMasterKey::generate();
    let (_priv_a, pub_a) = tusk_core::sync::generate_device_key().unwrap();
    let (priv_b, _pub_b) = tusk_core::sync::generate_device_key().unwrap();

    let wrap = wrap_rmk_for_device(&rmk, REPO_ID, &pub_a).unwrap();
    // A different device's private key cannot unwrap.
    assert!(unwrap_rmk_for_device(&wrap, REPO_ID, &priv_b).is_err());
    // The right device but the wrong repo id (AAD) cannot unwrap.
    let (priv_a2, pub_a2) = tusk_core::sync::generate_device_key().unwrap();
    let wrap2 = wrap_rmk_for_device(&rmk, REPO_ID, &pub_a2).unwrap();
    assert!(unwrap_rmk_for_device(&wrap2, "repo-OTHER", &priv_a2).is_err());
}

#[test]
fn device_wrap_does_not_contain_rmk() {
    let rmk = RepoMasterKey::generate();
    let (_private_pem, public_pem) = tusk_core::sync::generate_device_key().unwrap();
    let wrap = wrap_rmk_for_device(&rmk, REPO_ID, &public_pem).unwrap();
    // The serialized wrap must not embed the RMK in either public encoding.
    let json = serde_json::to_string(&wrap).unwrap();
    let otsk_hex = &rmk.to_otsk()[5..69]; // 64 hex chars of raw key
    assert!(!json.contains(otsk_hex));
}

// ---------------------------------------------------------------------------
// Blob naming
// ---------------------------------------------------------------------------

#[test]
fn blob_names_are_deterministic_and_structure_blind() {
    let rmk = RepoMasterKey::generate();
    let name = rmk.blob_name("memory/team/build-notes.md").unwrap();

    // Deterministic (overwrites hit the same slot), 64 lowercase hex chars.
    assert_eq!(rmk.blob_name("memory/team/build-notes.md").unwrap(), name);
    assert_eq!(name.len(), 64);
    assert!(name
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

    // No path structure leaks into the name.
    for part in ["memory", "team", "build-notes", ".md", "/"] {
        assert!(!name.contains(part), "name leaks {part:?}");
    }

    // Different paths and different RMKs give different names.
    assert_ne!(rmk.blob_name("memory/team/build-notes2.md").unwrap(), name);
    let other = RepoMasterKey::generate();
    assert_ne!(other.blob_name("memory/team/build-notes.md").unwrap(), name);
}

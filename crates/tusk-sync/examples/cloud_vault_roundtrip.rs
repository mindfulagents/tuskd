//! M1 rehearsal: the full encrypted vault pipeline over a real tusk-cloud —
//! the M0 round-trip (see tests/m0_roundtrip.rs) with `CloudProvider` in
//! place of `LocalProvider`.
//!
//! Encrypts a scratch vault under a fresh RMK, pushes blobs + sealed
//! manifest through presigned URLs, audits everything at rest server-side
//! (opaque names, no plaintext bytes), then recovers the RMK twice — from
//! the 24-word phrase and from a device wrap — pulls, decrypts, and
//! compares byte-for-byte. Cleans up its blobs afterwards.
//!
//! One device key authenticates both simulated installs: true multi-device
//! auth waits on the device-approval endpoints (next server slice).
//!
//! ```sh
//! TUSK_CLOUD_URL=... TUSK_REPO_ID=... TUSK_DEVICE_ID=... \
//! TUSK_KEY_SEED_HEX=... cargo run -p tusk-sync --example cloud_vault_roundtrip
//! ```

use std::collections::BTreeMap;
use tusk_sync::crypto::{KeyTable, RepoMasterKey};
use tusk_sync::wrap::{unwrap_rmk_for_device, wrap_rmk_for_device};
use tusk_sync::{CloudClient, CloudProvider, StorageProvider};

const MANIFEST: &str = "manifest";
const SECRETS: &[&str] = &[
    "TOP-SECRET-ALPHA-cloud-launch-plan",
    "TOP-SECRET-BETA-cloud-personnel-note",
];

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing env {name}"))
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn main() {
    let seed: [u8; 32] = hex::decode(env("TUSK_KEY_SEED_HEX"))
        .expect("hex seed")
        .try_into()
        .expect("32 bytes");
    let device_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let repo_id = env("TUSK_REPO_ID");
    let client = CloudClient::new(
        env("TUSK_CLOUD_URL"),
        &repo_id,
        env("TUSK_DEVICE_ID"),
        device_key,
    )
    .expect("client");
    let provider = CloudProvider::new(client).expect("provider");

    // --- Device A: scratch vault, encrypt under a fresh RMK, push. -------
    let mut originals: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    originals.insert(
        "memory/team/alpha.md".into(),
        format!("---\nscope: team\n---\n{}\n", SECRETS[0]).into_bytes(),
    );
    originals.insert(
        "memory/personal/beta.md".into(),
        format!("---\nscope: personal\n---\n{}\n", SECRETS[1]).into_bytes(),
    );
    originals.insert(".tusk/queue/review.json".into(), (0u8..=255).collect());

    let rmk = RepoMasterKey::generate();
    let phrase = rmk.to_mnemonic().expect("mnemonic");
    let mut table = KeyTable::new();
    for (rel, plaintext) in &originals {
        let entry = table.entry(&rmk, rel).expect("table entry");
        let blob = entry
            .dek()
            .expect("dek")
            .encrypt(&repo_id, rel, plaintext)
            .expect("encrypt");
        provider.put(&entry.blob, &blob).expect("push blob");
    }
    provider
        .put(MANIFEST, &table.seal(&rmk, &repo_id).expect("seal"))
        .expect("push manifest");
    println!("pushed {} encrypted objects + manifest", originals.len());

    // Wrap the RMK to a brand-new device key (the approval flow's payload).
    let (b_private_pem, b_public_pem) = tusk_core::sync::generate_device_key().expect("device key");
    let wrap = wrap_rmk_for_device(&rmk, &repo_id, &b_public_pem).expect("wrap");

    // --- Server-blindness audit over everything at rest. ------------------
    let names = provider.list().expect("list");
    let ours: Vec<_> = names
        .iter()
        .filter(|n| *n == MANIFEST || table.iter().any(|(_, e)| &e.blob == *n))
        .collect();
    assert!(ours.len() > originals.len(), "pushed blobs not listed");
    for name in &ours {
        if *name != MANIFEST {
            assert_eq!(name.len(), 64, "blob name shape");
            assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        }
        let bytes = provider.get(name).expect("fetch stored bytes");
        for secret in SECRETS {
            assert!(
                !contains_subslice(&bytes, secret.as_bytes()),
                "stored {name} leaks a secret"
            );
        }
        for (rel, plain) in &originals {
            assert!(!contains_subslice(&bytes, rel.as_bytes()));
            if plain.len() >= 16 {
                for window in plain.windows(16).step_by(8) {
                    assert!(!contains_subslice(&bytes, window), "{name} leaks source");
                }
            }
        }
    }
    println!(
        "server-blindness audit ok over {} stored objects",
        ours.len()
    );

    // --- "Device B" path 1: recover from the Secret Key phrase. -----------
    let rmk_phrase = RepoMasterKey::from_mnemonic(&phrase).expect("from phrase");
    let sealed = provider.get(MANIFEST).expect("manifest");
    let table_b = KeyTable::open(&sealed, &rmk_phrase, &repo_id).expect("open manifest");
    let mut pulled = BTreeMap::new();
    for (rel, entry) in table_b.iter() {
        let blob = provider.get(&entry.blob).expect("pull blob");
        let plaintext = entry
            .dek()
            .expect("dek")
            .decrypt(&repo_id, rel, &blob)
            .expect("decrypt");
        pulled.insert(rel.clone(), plaintext);
    }
    assert_eq!(pulled, originals, "phrase-recovered vault differs");
    println!("phrase recovery: byte-identical vault");

    // --- "Device B" path 2: recover via the device wrap. -------------------
    let rmk_wrap = unwrap_rmk_for_device(&wrap, &repo_id, &b_private_pem).expect("unwrap");
    let table_w = KeyTable::open(
        &provider.get(MANIFEST).expect("manifest"),
        &rmk_wrap,
        &repo_id,
    )
    .expect("open via wrap");
    let (rel, entry) = table_w.iter().next().expect("an entry");
    let blob = provider.get(&entry.blob).expect("pull blob");
    let plaintext = entry
        .dek()
        .expect("dek")
        .decrypt(&repo_id, rel, &blob)
        .expect("decrypt");
    assert_eq!(&plaintext, originals.get(rel).expect("original"));
    println!("wrap recovery: decrypts correctly");

    // --- Cleanup: tombstone what we pushed (exercises delete). ------------
    for (_, entry) in table.iter() {
        provider.delete(&entry.blob).expect("delete blob");
    }
    provider.delete(MANIFEST).expect("delete manifest");
    println!("cleanup ok");
    println!("cloud vault roundtrip: PASS");
}

//! The M1 exit-test flow with two real device identities (D25):
//!
//! Device A (existing, credentials from env) pushes an encrypted vault.
//! Device B generates its own keypair locally, **enrolls** (pending, no
//! access), A **lists** devices and checks B's fingerprint, wraps the RMK
//! to B's key on-device and **approves**; B **fetches its wrap**, unwraps
//! the RMK locally, and pulls the vault byte-identical — authenticated
//! throughout as itself, never sharing A's key. B then pushes an op that
//! A pulls and signature-verifies against B's listed pubkey.
//!
//! The server relays only public keys and opaque ciphertext at every step.
//!
//! ```sh
//! TUSK_CLOUD_URL=... TUSK_REPO_ID=... TUSK_DEVICE_ID=... \
//! TUSK_KEY_SEED_HEX=... cargo run -p tusk-sync --example two_device_sync
//! ```

use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, DecodePrivateKey, EncodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::collections::BTreeMap;
use tusk_sync::crypto::{KeyTable, RepoMasterKey};
use tusk_sync::wrap::{unwrap_rmk_for_device, wrap_rmk_for_device};
use tusk_sync::{
    device_fingerprint, enroll_device, verify_op, CloudClient, CloudProvider, StorageProvider,
};

const MANIFEST: &str = "manifest";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing env {name}"))
}

fn main() {
    let base_url = env("TUSK_CLOUD_URL");
    let repo_id = env("TUSK_REPO_ID");

    // --- Device A: existing identity, pushes an encrypted vault. ----------
    let seed: [u8; 32] = hex::decode(env("TUSK_KEY_SEED_HEX"))
        .expect("hex seed")
        .try_into()
        .expect("32 bytes");
    let key_a = SigningKey::from_bytes(&seed);
    let client_a = CloudClient::new(&base_url, &repo_id, env("TUSK_DEVICE_ID"), key_a.clone())
        .expect("client A");
    let provider_a = CloudProvider::new(
        CloudClient::new(&base_url, &repo_id, env("TUSK_DEVICE_ID"), key_a).expect("client"),
    )
    .expect("provider A");

    let mut originals: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    originals.insert(
        "memory/team/two-device.md".into(),
        b"TOP-SECRET-two-device-payload\n".to_vec(),
    );
    originals.insert(".tusk/queue/binary.bin".into(), (0u8..=255).collect());

    let rmk = RepoMasterKey::generate();
    let mut table = KeyTable::new();
    for (rel, plaintext) in &originals {
        let entry = table.entry(&rmk, rel).expect("entry");
        let blob = entry
            .dek()
            .expect("dek")
            .encrypt(&repo_id, rel, plaintext)
            .expect("encrypt");
        provider_a.put(&entry.blob, &blob).expect("A push blob");
    }
    provider_a
        .put(MANIFEST, &table.seal(&rmk, &repo_id).expect("seal"))
        .expect("A push manifest");
    println!("A: pushed {} encrypted objects + manifest", originals.len());

    // --- Device B: fresh identity, enrolls itself. ------------------------
    let (b_private_pem, b_public_pem) = tusk_core::sync::generate_device_key().expect("B keypair");
    let b_signing = SigningKey::from_pkcs8_pem(&b_private_pem).expect("B signing key");
    let b_raw_pub = b_signing.verifying_key().to_bytes();
    let b_x25519 = b_signing.verifying_key().to_montgomery().to_bytes();
    let (b_device_id, b_fingerprint) =
        enroll_device(&base_url, &repo_id, "device-b", &b_raw_pub, &b_x25519).expect("enroll B");
    println!("B: enrolled pending, fingerprint {b_fingerprint}");

    // B is pending: its own client must be refused (403) for now.
    let client_b =
        CloudClient::new(&base_url, &repo_id, &b_device_id, b_signing.clone()).expect("client B");
    match client_b.fetch_wrap() {
        Err(tusk_sync::SyncError::Http { status: 403, .. }) => {
            println!("B: correctly refused while pending (403)")
        }
        other => panic!("pending B should be 403, got {other:?}"),
    }

    // --- A: list, verify fingerprint out-of-band, wrap + approve. ---------
    let devices = client_a.list_devices().expect("A lists devices");
    let b_row = devices
        .iter()
        .find(|d| d.device_id == b_device_id)
        .expect("B listed");
    assert_eq!(b_row.status, "pending");
    assert_eq!(
        b_row.fingerprint, b_fingerprint,
        "fingerprints must match on both sides"
    );
    // A reconstructs B's public PEM from the listed raw key — the server
    // never handles PEMs, only raw public bytes.
    use base64::Engine;
    let listed_raw: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(&b_row.ed25519_pubkey)
        .expect("b64")
        .try_into()
        .expect("32 bytes");
    let b_pem_rebuilt = VerifyingKey::from_bytes(&listed_raw)
        .expect("valid key")
        .to_public_key_pem(LineEnding::LF)
        .expect("pem");
    assert_eq!(b_pem_rebuilt.trim(), b_public_pem.trim());
    let wrap = wrap_rmk_for_device(&rmk, &repo_id, &b_pem_rebuilt).expect("wrap RMK to B");
    let wrap_bytes = serde_json::to_vec(&wrap).expect("serialize wrap");
    client_a
        .approve_device(&b_device_id, &wrap_bytes, 1)
        .expect("A approves B");
    println!("A: verified fingerprint, wrapped RMK to B, approved");

    // --- B: fetch wrap, unwrap locally, pull the vault as itself. ---------
    let (fetched_wrap, generation) = client_b.fetch_wrap().expect("B fetches wrap");
    assert_eq!(generation, 1);
    let wrap: tusk_sync::DeviceWrap =
        serde_json::from_slice(&fetched_wrap).expect("deserialize wrap");
    let rmk_b = unwrap_rmk_for_device(&wrap, &repo_id, &b_private_pem).expect("B unwraps RMK");

    let provider_b = CloudProvider::new(
        CloudClient::new(&base_url, &repo_id, &b_device_id, b_signing.clone()).expect("client"),
    )
    .expect("provider B");
    let sealed = provider_b.get(MANIFEST).expect("B pulls manifest");
    let table_b = KeyTable::open(&sealed, &rmk_b, &repo_id).expect("B opens manifest");
    let mut pulled = BTreeMap::new();
    for (rel, entry) in table_b.iter() {
        let blob = provider_b.get(&entry.blob).expect("B pulls blob");
        let plaintext = entry
            .dek()
            .expect("dek")
            .decrypt(&repo_id, rel, &blob)
            .expect("B decrypts");
        pulled.insert(rel.clone(), plaintext);
    }
    assert_eq!(pulled, originals, "B's vault differs from A's");
    println!("B: pulled and decrypted byte-identical vault as itself");

    // --- B pushes an op; A pulls and verifies it against B's pubkey. ------
    let seq = client_b.append_op(b"hello-from-b").expect("B appends op");
    let ops = client_a.ops_since(seq - 1, Some(10)).expect("A pulls");
    let op = ops
        .iter()
        .find(|op| op.seq == seq)
        .expect("B's op visible to A");
    assert_eq!(op.device_id, b_device_id);
    let b_key_from_list = VerifyingKey::from_bytes(&listed_raw).expect("key");
    verify_op(&b_key_from_list, &repo_id, &op.payload, &op.signature)
        .expect("A verifies B's op signature end-to-end");
    println!("A: pulled B's op seq={seq} and verified its signature");
    assert_eq!(device_fingerprint(&b_raw_pub), b_fingerprint);

    // --- Cleanup. ----------------------------------------------------------
    for (_, entry) in table.iter() {
        provider_b.delete(&entry.blob).expect("cleanup blob");
    }
    provider_b.delete(MANIFEST).expect("cleanup manifest");
    println!("cleanup ok");
    println!("two-device sync: PASS");
}

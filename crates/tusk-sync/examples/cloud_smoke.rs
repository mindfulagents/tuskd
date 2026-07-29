//! Smoke-drive a tusk-cloud instance with the real `CloudClient` (D23).
//!
//! Reads the target from env, appends two ops, pulls them back, verifies
//! their signatures against our own key, and lists blobs. Exits non-zero on
//! any mismatch. Used for the M1 cross-repo E2E against a locally-run
//! tusk-cloud and, later, against cloud.opentusk.ai.
//!
//! ```sh
//! TUSK_CLOUD_URL=http://127.0.0.1:7801 \
//! TUSK_REPO_ID=<uuid> TUSK_DEVICE_ID=<uuid> TUSK_KEY_SEED_HEX=<64 hex> \
//! cargo run -p tusk-sync --example cloud_smoke
//! ```

use ed25519_dalek::SigningKey;
use tusk_sync::cloud::verify_op;
use tusk_sync::CloudClient;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing env {name}"))
}

fn main() {
    let base_url = env("TUSK_CLOUD_URL");
    let repo_id = env("TUSK_REPO_ID");
    let device_id = env("TUSK_DEVICE_ID");
    let seed: [u8; 32] = hex::decode(env("TUSK_KEY_SEED_HEX"))
        .expect("TUSK_KEY_SEED_HEX is hex")
        .try_into()
        .expect("seed is 32 bytes");
    let key = SigningKey::from_bytes(&seed);

    let client = CloudClient::new(&base_url, &repo_id, &device_id, key.clone()).expect("client");

    let first = client.append_op(b"smoke-op-1").expect("append op 1");
    let second = client.append_op(b"smoke-op-2").expect("append op 2");
    println!("appended ops seq={first},{second}");
    assert!(second > first, "seqs must be monotonic");

    let ops = client.ops_since(0, Some(100)).expect("pull ops");
    println!("pulled {} ops", ops.len());
    let mine: Vec<_> = ops.iter().filter(|op| op.device_id == device_id).collect();
    assert!(mine.len() >= 2, "expected at least our two ops");
    for op in &mine {
        verify_op(&key.verifying_key(), &repo_id, &op.payload, &op.signature)
            .expect("pulled op signature verifies against our device key");
    }
    println!("all {} of our ops verify end-to-end", mine.len());

    let blobs = client.list_blobs().expect("list blobs");
    println!("blob list ok ({} blobs)", blobs.len());

    // Blob data path (C7): presign put -> raw HTTP PUT -> record -> presign
    // get -> raw HTTP GET -> byte compare. Skips cleanly when the server
    // has no blob store configured (503).
    let payload = b"smoke-blob-payload";
    match client.presign_put("smoke.blob", payload.len() as u64) {
        Err(tusk_sync::SyncError::Http { status: 503, .. }) => {
            println!("blob data path: SKIPPED (server has no blob store configured)");
        }
        Err(err) => panic!("presign put failed: {err}"),
        Ok(put) => {
            let http = reqwest::blocking::Client::new();
            let resp = http
                .put(&put.url)
                .body(payload.to_vec())
                .send()
                .expect("PUT to store");
            assert!(resp.status().is_success(), "PUT failed: {}", resp.status());
            client
                .record_blob("smoke.blob", payload.len() as u64)
                .expect("record blob");
            let listed = client.list_blobs().expect("list after record");
            assert!(listed.iter().any(|name| name == "smoke.blob"));
            let get = client.presign_get("smoke.blob").expect("presign get");
            let bytes = http
                .get(&get.url)
                .send()
                .expect("GET from store")
                .bytes()
                .expect("body");
            assert_eq!(&bytes[..], payload, "downloaded bytes differ");
            println!("blob data path: round-trip ok ({} bytes)", bytes.len());
        }
    }
    println!("cloud smoke: PASS");
}

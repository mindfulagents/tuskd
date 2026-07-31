//! D28 auto-sync worker E2E: two (then three) devices syncing through a
//! stateful in-process fake of the tusk-cloud `/v1` surface plus blob
//! store. The fake serves the same JSON shapes as tusk-cloud (C3–C9) but
//! skips signature *checking* — the client's signatures are covered by
//! tusk-sync's m1_cloud tests and were verified live; here we exercise the
//! sync semantics: incremental push, oplog-driven pull, local-wins
//! conflicts, deletion propagation, fresh-device adoption, and the
//! rotation re-key pass.

use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, EncodePrivateKey};
use ed25519_dalek::SigningKey;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tusk_sync::crypto::RepoMasterKey;
use tuskd::sync_worker::{cycle, pull_all, push_only};

const REPO: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

// ---------------------------------------------------------------------------
// Stateful fake cloud
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeCloud {
    /// (seq, device_id, payload_b64, signature_b64)
    ops: Vec<(i64, String, String, String)>,
    blobs: BTreeMap<String, Vec<u8>>,
    devices: Vec<serde_json::Value>,
    /// Counters the tests assert on.
    blob_puts: usize,
    manifest_puts: usize,
}

struct Server {
    state: Arc<Mutex<FakeCloud>>,
    url: String,
    _keep: std::sync::mpsc::Sender<()>, // dropping stops the accept loop
}

fn spawn_server() -> Server {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}");
    let state: Arc<Mutex<FakeCloud>> = Arc::default();
    let (keep, alive) = std::sync::mpsc::channel::<()>();
    let thread_state = Arc::clone(&state);
    let thread_url = url.clone();
    std::thread::spawn(move || {
        listener.set_nonblocking(false).expect("blocking listener");
        loop {
            // Stop serving once the test drops its handle.
            if let Err(std::sync::mpsc::TryRecvError::Disconnected) = alive.try_recv() {
                break;
            }
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let (method, path, body) = read_request(&mut stream);
            let (status, response) = handle(&thread_state, &thread_url, &method, &path, &body);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    response.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(&response);
        }
    });
    Server {
        state,
        url,
        _keep: keep,
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> (String, String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).expect("read");
        assert!(n > 0, "peer closed before headers complete");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let start = lines.next().expect("start line");
    let mut parts = start.split(' ');
    let method = parts.next().expect("method").to_string();
    let path = parts.next().expect("path").to_string();
    let content_length: usize = lines
        .filter_map(|l| l.split_once(": "))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.parse().expect("length"))
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "peer closed mid-body");
        body.extend_from_slice(&chunk[..n]);
    }
    (method, path, body)
}

fn handle(
    state: &Arc<Mutex<FakeCloud>>,
    base_url: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> (&'static str, Vec<u8>) {
    let mut fake = state.lock().expect("fake lock");
    let ops_path = format!("/v1/repos/{REPO}/ops");
    let blobs_path = format!("/v1/repos/{REPO}/blobs");
    let (bare_path, query) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    };

    // Raw blob store (what presigned URLs point at).
    if let Some(name) = bare_path.strip_prefix("/raw/") {
        return match method {
            "PUT" => {
                fake.blobs.insert(name.to_string(), body.to_vec());
                ("200 OK", b"{}".to_vec())
            }
            "GET" => match fake.blobs.get(name) {
                Some(bytes) => ("200 OK", bytes.clone()),
                None => ("404 Not Found", b"{}".to_vec()),
            },
            _ => ("405 Method Not Allowed", b"{}".to_vec()),
        };
    }

    if bare_path == ops_path && method == "POST" {
        let parsed: serde_json::Value = serde_json::from_slice(body).expect("op json");
        let seq = fake.ops.len() as i64 + 1;
        fake.ops.push((
            seq,
            parsed["device_id"].as_str().expect("device").to_string(),
            parsed["payload"].as_str().expect("payload").to_string(),
            parsed["signature"].as_str().expect("sig").to_string(),
        ));
        return ("200 OK", format!(r#"{{"seq":{seq}}}"#).into_bytes());
    }
    if bare_path == ops_path && method == "GET" {
        let since: i64 = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("since="))
            .map(|v| v.parse().expect("since"))
            .unwrap_or(0);
        let limit: usize = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("limit="))
            .map(|v| v.parse().expect("limit"))
            .unwrap_or(usize::MAX);
        let ops: Vec<serde_json::Value> = fake
            .ops
            .iter()
            .filter(|(seq, ..)| *seq > since)
            .take(limit)
            .map(|(seq, device, payload, signature)| {
                serde_json::json!({
                    "seq": seq, "device_id": device,
                    "payload": payload, "signature": signature,
                })
            })
            .collect();
        return (
            "200 OK",
            serde_json::json!({ "ops": ops }).to_string().into_bytes(),
        );
    }
    if bare_path == format!("{blobs_path}/presign") && method == "POST" {
        let parsed: serde_json::Value = serde_json::from_slice(body).expect("presign json");
        let name = parsed["name"].as_str().expect("name");
        if parsed["op"] == "get" && !fake.blobs.contains_key(name) {
            return ("404 Not Found", b"{}".to_vec());
        }
        if parsed["op"] == "put" {
            if name == "manifest" {
                fake.manifest_puts += 1;
            } else {
                fake.blob_puts += 1;
            }
        }
        let url = format!("{base_url}/raw/{name}");
        return (
            "200 OK",
            serde_json::json!({ "url": url, "expires_secs": 300 })
                .to_string()
                .into_bytes(),
        );
    }
    if bare_path == blobs_path && method == "POST" {
        return ("200 OK", b"{}".to_vec()); // record_blob
    }
    if bare_path == blobs_path && method == "GET" {
        let names: Vec<&String> = fake.blobs.keys().collect();
        return ("200 OK", serde_json::to_vec(&names).expect("names json"));
    }
    if let Some(name) = bare_path.strip_prefix(&format!("{blobs_path}/")) {
        if method == "DELETE" {
            fake.blobs.remove(name);
            return ("200 OK", b"{}".to_vec());
        }
    }
    if bare_path == format!("/v1/repos/{REPO}/devices") && method == "GET" {
        return (
            "200 OK",
            serde_json::to_vec(&fake.devices).expect("devices json"),
        );
    }
    panic!("fake cloud: unhandled {method} {path}");
}

// ---------------------------------------------------------------------------
// Device/vault setup
// ---------------------------------------------------------------------------

struct Device {
    vault: tempfile::TempDir,
    device_id: String,
}

impl Device {
    fn path(&self) -> &std::path::Path {
        self.vault.path()
    }
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path().join(rel)).ok()
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.path().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }
}

/// Provision an approved device: vault dir with `.tusk/sync/` holding
/// cloud.json, device.pem, rmk.otsk (+gen), registered in the fake's
/// device list — the state a real device is in after connect + approve.
fn provision(server: &Server, device_id: &str, rmk: &RepoMasterKey, seed: u8) -> Device {
    let vault = tempfile::tempdir().expect("vault dir");
    let sync = vault.path().join(".tusk").join("sync");
    std::fs::create_dir_all(&sync).expect("sync dir");
    let key = SigningKey::from_bytes(&[seed; 32]);
    let pem = key.to_pkcs8_pem(LineEnding::LF).expect("pem").to_string();
    std::fs::write(sync.join("device.pem"), pem).expect("device.pem");
    std::fs::write(
        sync.join("cloud.json"),
        serde_json::json!({
            "url": server.url, "repo_id": REPO, "device_id": device_id,
        })
        .to_string(),
    )
    .expect("cloud.json");
    std::fs::write(sync.join("rmk.otsk"), rmk.to_otsk()).expect("rmk");
    std::fs::write(sync.join("rmk.gen"), "1").expect("gen");

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    server
        .state
        .lock()
        .expect("lock")
        .devices
        .push(serde_json::json!({
            "device_id": device_id,
            "name": format!("dev-{seed}"),
            "status": "approved",
            "ed25519_pubkey": b64.encode(key.verifying_key().to_bytes()),
            "x25519_pubkey": b64.encode(key.verifying_key().to_montgomery().to_bytes()),
            "fingerprint": tusk_sync::device_fingerprint(&key.verifying_key().to_bytes()),
        }));
    Device {
        vault,
        device_id: device_id.to_string(),
    }
}

fn counters(server: &Server) -> (usize, usize, usize) {
    let fake = server.state.lock().expect("lock");
    (fake.ops.len(), fake.blob_puts, fake.manifest_puts)
}

// ---------------------------------------------------------------------------
// The scenarios (one flow — order matters, so a single test)
// ---------------------------------------------------------------------------

#[test]
fn two_devices_sync_incrementally_with_conflicts_deletes_and_rekey() {
    let server = spawn_server();
    let rmk = RepoMasterKey::from_otsk(&RepoMasterKey::generate().to_otsk()).expect("rmk");
    let a = provision(&server, "aaaaaaaa-0000-0000-0000-000000000001", &rmk, 1);
    let b = provision(&server, "bbbbbbbb-0000-0000-0000-000000000002", &rmk, 2);

    // --- A pushes its initial content; B adopts it. -----------------------
    a.write("memory/org/greeting.md", b"hello from A\n");
    a.write("memory/org/shared.md", b"v1\n");
    let report = cycle(a.path()).expect("A first cycle");
    assert_eq!(report.pushed, 2);
    assert_eq!(report.pulled, 0);

    let report = cycle(b.path()).expect("B first cycle");
    assert_eq!(report.pulled, 2, "B adopts A's files");
    assert_eq!(report.pushed, 0, "B has nothing of its own");
    assert_eq!(b.read("memory/org/greeting.md").unwrap(), b"hello from A\n");

    // --- Idle cycles are true no-ops. -------------------------------------
    let before = counters(&server);
    assert!(cycle(a.path()).expect("A idle").is_noop());
    assert!(cycle(b.path()).expect("B idle").is_noop());
    assert_eq!(counters(&server), before, "idle cycles hit no write path");

    // --- Content change: one blob uploaded, manifest untouched. -----------
    let (_, _, manifest_puts_before) = counters(&server);
    a.write("memory/org/greeting.md", b"hello again from A\n");
    let report = cycle(a.path()).expect("A push edit");
    assert_eq!(report.pushed, 1);
    let (_, _, manifest_puts_after) = counters(&server);
    assert_eq!(
        manifest_puts_before, manifest_puts_after,
        "editing an existing file must not re-seal the manifest"
    );
    let report = cycle(b.path()).expect("B pull edit");
    assert_eq!(report.pulled, 1);
    assert_eq!(
        b.read("memory/org/greeting.md").unwrap(),
        b"hello again from A\n"
    );

    // --- Conflict: both edit; the device with unsynced local changes keeps
    // --- its copy and re-pushes; everyone converges on it. ----------------
    a.write("memory/org/shared.md", b"A's version\n");
    b.write("memory/org/shared.md", b"B's version\n");
    let report = cycle(a.path()).expect("A push conflict");
    assert_eq!(report.pushed, 1);
    let report = cycle(b.path()).expect("B conflict cycle");
    assert_eq!(
        report.pulled, 0,
        "local wins: A's copy must not overwrite B's"
    );
    assert_eq!(report.pushed, 1, "B re-pushes its own copy");
    assert_eq!(b.read("memory/org/shared.md").unwrap(), b"B's version\n");
    let report = cycle(a.path()).expect("A converges");
    assert_eq!(report.pulled, 1);
    assert_eq!(a.read("memory/org/shared.md").unwrap(), b"B's version\n");

    // --- Deletion propagates (only to clean copies). ----------------------
    std::fs::remove_file(a.path().join("memory/org/shared.md")).expect("rm");
    let report = cycle(a.path()).expect("A push delete");
    assert_eq!(report.deleted_remote, 1);
    let report = cycle(b.path()).expect("B pull delete");
    assert_eq!(report.deleted_local, 1);
    assert!(b.read("memory/org/shared.md").is_none());

    // --- A fresh, empty device adopts everything and tombstones nothing. --
    let (ops_before, ..) = counters(&server);
    let c = provision(&server, "cccccccc-0000-0000-0000-000000000003", &rmk, 3);
    let report = cycle(c.path()).expect("C first cycle");
    assert_eq!(report.pulled, 1);
    assert_eq!(
        report.deleted_remote, 0,
        "an empty vault must never mass-delete"
    );
    assert_eq!(
        c.read("memory/org/greeting.md").unwrap(),
        b"hello again from A\n"
    );
    let (ops_after, ..) = counters(&server);
    assert_eq!(ops_before, ops_after, "adoption appends no ops");

    // --- Rotation re-key (D27 via D28): simulate a rotation performed
    // --- elsewhere: manifest re-sealed under a new RMK, all vaults handed
    // --- the new key at generation 2. -------------------------------------
    let rmk2 = RepoMasterKey::generate();
    {
        let sealed = {
            let fake = server.state.lock().expect("lock");
            fake.blobs.get("manifest").expect("manifest").clone()
        };
        let table = tusk_sync::KeyTable::open(&sealed, &rmk, REPO).expect("open manifest");
        let resealed = table.seal(&rmk2, REPO).expect("re-seal");
        server
            .state
            .lock()
            .expect("lock")
            .blobs
            .insert("manifest".into(), resealed);
    }
    for device in [&a, &b, &c] {
        let sync = device.path().join(".tusk").join("sync");
        std::fs::write(sync.join("rmk.otsk"), rmk2.to_otsk()).expect("rmk2");
        std::fs::write(sync.join("rmk.gen"), "2").expect("gen2");
    }
    let old_names: Vec<String> = {
        let fake = server.state.lock().expect("lock");
        fake.blobs
            .keys()
            .filter(|n| *n != "manifest")
            .cloned()
            .collect()
    };
    let report = cycle(a.path()).expect("A re-key cycle");
    assert_eq!(report.pushed, 1, "every blob re-encrypted at its new name");
    {
        let fake = server.state.lock().expect("lock");
        for name in &old_names {
            assert!(
                !fake.blobs.contains_key(name),
                "old-generation blob name {name} must be tombstoned"
            );
        }
    }
    // The other devices find nothing left to re-key, and content survives.
    let report = cycle(b.path()).expect("B post-rotation");
    assert_eq!(report.pushed, 0, "re-key is idempotent across devices");
    assert_eq!(
        b.read("memory/org/greeting.md").unwrap(),
        b"hello again from A\n"
    );

    // Manual verbs ride the same state: push reports in-sync, pull re-materializes.
    let report = push_only(a.path()).expect("manual push");
    assert!(report.is_noop());
    let written = pull_all(b.path()).expect("manual pull");
    assert_eq!(written, 1);

    // Silence "unused field" — device ids are part of provisioning.
    assert_ne!(a.device_id, b.device_id);
}

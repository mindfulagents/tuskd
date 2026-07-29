//! `SpacesProvider` against a minimal local HTTP blob server on an ephemeral
//! port (127.0.0.1:0 — never the live daemon, never a real network). The
//! server plays the role of DO Spaces + tusk-cloud's list endpoint; the
//! provider only ever sees URLs, exactly as with M1 presigns.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use tusk_sync::{BlobOp, PresignSource, SpacesProvider, StorageProvider, SyncError};

type Blobs = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// Serve one connection: `PUT/GET/DELETE /blobs/<name>` + `GET /list`.
fn handle(mut stream: TcpStream, blobs: &Blobs) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).unwrap();
    }

    let (status, resp_body): (&str, Vec<u8>) = {
        let mut store = blobs.lock().unwrap();
        match (method.as_str(), path.as_str()) {
            ("GET", "/list") => {
                let mut names: Vec<&String> = store.keys().collect();
                names.sort();
                ("200 OK", serde_json::to_vec(&names).unwrap())
            }
            (m, p) if p.starts_with("/blobs/") => {
                let name = p.trim_start_matches("/blobs/").to_string();
                match m {
                    "PUT" => {
                        store.insert(name, body);
                        ("200 OK", Vec::new())
                    }
                    "GET" => match store.get(&name) {
                        Some(bytes) => ("200 OK", bytes.clone()),
                        None => ("404 Not Found", Vec::new()),
                    },
                    "DELETE" => {
                        store.remove(&name);
                        ("204 No Content", Vec::new())
                    }
                    _ => ("405 Method Not Allowed", Vec::new()),
                }
            }
            _ => ("404 Not Found", Vec::new()),
        }
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp_body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&resp_body);
}

/// Bind 127.0.0.1:0, serve connections on a background thread, return the
/// base URL. The thread dies with the test process.
fn spawn_server() -> (String, Blobs) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let blobs: Blobs = Arc::new(Mutex::new(HashMap::new()));
    let server_blobs = Arc::clone(&blobs);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            handle(stream, &server_blobs);
        }
    });
    (format!("http://{addr}"), blobs)
}

/// Test presign source: URL construction only, mirroring what tusk-cloud
/// hands out in M1. The provider itself signs nothing either way.
struct TestPresigns {
    base: String,
}

impl PresignSource for TestPresigns {
    fn url_for(&self, op: BlobOp, name: &str) -> Result<String, SyncError> {
        Ok(match op {
            BlobOp::List => format!("{}/list", self.base),
            _ => format!("{}/blobs/{name}", self.base),
        })
    }
}

#[test]
fn spaces_provider_round_trip_over_http() {
    let (base, raw_store) = spawn_server();
    let provider = SpacesProvider::new(TestPresigns { base }).unwrap();

    // put / get / list / overwrite / delete (idempotent) — same contract the
    // LocalProvider tests pin down.
    provider.put("aa11", b"cipher-one").unwrap();
    provider.put("bb22", &[0u8, 159, 146, 150, 255]).unwrap(); // non-UTF8 body
    assert_eq!(provider.get("aa11").unwrap(), b"cipher-one");
    assert_eq!(provider.get("bb22").unwrap(), vec![0u8, 159, 146, 150, 255]);
    assert_eq!(
        provider.list().unwrap(),
        vec!["aa11".to_string(), "bb22".to_string()]
    );

    provider.put("aa11", b"cipher-one-v2").unwrap();
    assert_eq!(provider.get("aa11").unwrap(), b"cipher-one-v2");

    assert!(matches!(provider.get("cc33"), Err(SyncError::NotFound(_))));
    provider.delete("bb22").unwrap();
    provider.delete("bb22").unwrap();
    assert_eq!(provider.list().unwrap(), vec!["aa11".to_string()]);

    // What the server holds is exactly what was put — opaque bytes.
    assert_eq!(
        raw_store.lock().unwrap().get("aa11").unwrap(),
        &b"cipher-one-v2".to_vec()
    );
}

#[test]
fn spaces_provider_encrypted_round_trip() {
    use tusk_sync::crypto::{KeyTable, RepoMasterKey};

    let (base, raw_store) = spawn_server();
    let provider = SpacesProvider::new(TestPresigns { base }).unwrap();

    let repo_id = "repo-01JZX7Y9QK5SPACES";
    let rel = "memory/team/http-note.md";
    let secret = b"TOP-SECRET-over-http";

    let rmk = RepoMasterKey::generate();
    let mut table = KeyTable::new();
    let entry = table.entry(&rmk, rel).unwrap();
    let blob = entry.dek().unwrap().encrypt(repo_id, rel, secret).unwrap();
    provider.put(&entry.blob, &blob).unwrap();
    provider
        .put("manifest", &table.seal(&rmk, repo_id).unwrap())
        .unwrap();

    // Server-side bytes are ciphertext only.
    for bytes in raw_store.lock().unwrap().values() {
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_slice()));
    }

    // "Second device": only the RMK and the provider.
    let table2 = KeyTable::open(&provider.get("manifest").unwrap(), &rmk, repo_id).unwrap();
    let entry2 = table2.get(rel).unwrap();
    let plain = entry2
        .dek()
        .unwrap()
        .decrypt(repo_id, rel, &provider.get(&entry2.blob).unwrap())
        .unwrap();
    assert_eq!(plain, secret);
}

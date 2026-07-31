//! M1 cloud-client tests: the C4 golden vectors (the cross-repo wire lock
//! with tusk-cloud) and full request/response round trips against a
//! hand-rolled single-shot HTTP server — every signature the client emits
//! is verified server-side in the test, exactly as tusk-cloud would.

use ed25519_dalek::{Signer, SigningKey, Verifier};
use std::io::{Read, Write};
use std::sync::mpsc;
use tusk_sync::cloud::{op_message, request_message, verify_op, CloudClient};

const SEED: [u8; 32] = [7u8; 32];
const REPO: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
const DEVICE: &str = "b0e42fe7-31a5-4894-a441-bf1e30cbd7d2";

/// Pinned in tusk-cloud's tests/c4_vectors.rs as well. Do not regenerate to
/// make a failing test pass — a mismatch means the wire format changed.
const VERIFYING_KEY_HEX: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
const OP_SIG_HEX: &str = "21a2345847e6de35dfb39ed6a2ec6a23a4dde38986cb152e2e760f2445fce688d131d01b6e8d7eaa4c76f19a5bacb826f1ccbede1fb7e68155176c2a5d1be308";
const REQ_SIG_HEX: &str = "03b4dc8a43a120c00d9b7332e89b1a6e2b5aedc612eba9a72091fae654abb6bec4eee4b7a692f5b114e2564c6c4f18ed7efa13515e1431521090dd88e87eca06";

fn key() -> SigningKey {
    SigningKey::from_bytes(&SEED)
}

#[test]
fn c4_op_signature_matches_pinned_vector() {
    assert_eq!(
        hex::encode(key().verifying_key().to_bytes()),
        VERIFYING_KEY_HEX
    );
    let sig = key().sign(&op_message(REPO, b"tusk-vector-payload"));
    assert_eq!(hex::encode(sig.to_bytes()), OP_SIG_HEX);
}

#[test]
fn c4_request_signature_matches_pinned_vector() {
    let sig = key().sign(&request_message(
        "GET",
        &format!("/v1/repos/{REPO}/ops"),
        1785000000,
    ));
    assert_eq!(hex::encode(sig.to_bytes()), REQ_SIG_HEX);
}

/// A captured HTTP request: start line, lowercased headers, body.
struct RawRequest {
    start_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawRequest {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("missing header {name}"))
    }
}

/// Serve exactly one request on an ephemeral port with a canned JSON body,
/// handing the captured request back through a channel.
fn one_shot_server(
    status_line: &'static str,
    body: &'static str,
) -> (String, mpsc::Receiver<RawRequest>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = read_request(&mut stream);
        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        tx.send(request).expect("send captured request");
    });
    (format!("http://{addr}"), rx)
}

fn read_request(stream: &mut std::net::TcpStream) -> RawRequest {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut chunk).expect("read");
        assert!(n > 0, "peer closed before headers complete");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8(buf[..header_end].to_vec()).expect("ascii head");
    let mut lines = head.split("\r\n");
    let start_line = lines.next().expect("start line").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
        .collect();
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .map(|(_, v)| v.parse().expect("content-length"))
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "peer closed mid-body");
        body.extend_from_slice(&chunk[..n]);
    }
    RawRequest {
        start_line,
        headers,
        body,
    }
}

fn b64_decode(value: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .expect("base64")
}

fn client(base_url: &str) -> CloudClient {
    CloudClient::new(base_url, REPO, DEVICE, key()).expect("client")
}

#[test]
fn append_op_sends_an_op_the_server_can_verify() {
    let (base, rx) = one_shot_server("200 OK", r#"{"seq":41}"#);
    let payload = b"opaque-ciphertext";
    let seq = client(&base).append_op(payload).expect("append");
    assert_eq!(seq, 41);

    let request = rx.recv().expect("captured request");
    assert_eq!(
        request.start_line,
        format!("POST /v1/repos/{REPO}/ops HTTP/1.1")
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(body["device_id"], DEVICE);
    let sent_payload = b64_decode(body["payload"].as_str().expect("payload"));
    assert_eq!(sent_payload, payload);

    // Server-side verification, exactly as tusk-cloud performs it.
    let signature = b64_decode(body["signature"].as_str().expect("signature"));
    verify_op(&key().verifying_key(), REPO, &sent_payload, &signature)
        .expect("op signature verifies against the device key");
}

#[test]
fn ops_since_signs_the_request_and_parses_ops() {
    let sig = key().sign(&op_message(REPO, b"pulled-payload"));
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let response: &'static str = Box::leak(
        format!(
            r#"{{"ops":[{{"seq":7,"device_id":"{DEVICE}","payload":"{}","signature":"{}"}}]}}"#,
            b64.encode(b"pulled-payload"),
            b64.encode(sig.to_bytes()),
        )
        .into_boxed_str(),
    );
    let (base, rx) = one_shot_server("200 OK", response);

    let ops = client(&base).ops_since(6, Some(10)).expect("pull");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].seq, 7);
    assert_eq!(ops[0].device_id, DEVICE);
    assert_eq!(ops[0].payload, b"pulled-payload");
    verify_op(
        &key().verifying_key(),
        REPO,
        &ops[0].payload,
        &ops[0].signature,
    )
    .expect("pulled op verifies");

    let request = rx.recv().expect("captured request");
    assert_eq!(
        request.start_line,
        format!("GET /v1/repos/{REPO}/ops?since=6&limit=10 HTTP/1.1")
    );
    assert_eq!(request.header("x-tusk-device"), DEVICE);
    let timestamp: i64 = request.header("x-tusk-timestamp").parse().expect("ts");
    let signature = b64_decode(request.header("x-tusk-signature"));
    // The signed path excludes the query string (C4).
    key()
        .verifying_key()
        .verify(
            &request_message("GET", &format!("/v1/repos/{REPO}/ops"), timestamp),
            &ed25519_dalek::Signature::from_bytes(
                signature.as_slice().try_into().expect("64 bytes"),
            ),
        )
        .expect("request signature verifies");
}

#[test]
fn list_blobs_parses_the_bare_json_array() {
    let (base, rx) = one_shot_server("200 OK", r#"["a.blob","b.blob"]"#);
    let names = client(&base).list_blobs().expect("list");
    assert_eq!(names, ["a.blob", "b.blob"]);

    let request = rx.recv().expect("captured request");
    assert_eq!(
        request.start_line,
        format!("GET /v1/repos/{REPO}/blobs HTTP/1.1")
    );
    let timestamp: i64 = request.header("x-tusk-timestamp").parse().expect("ts");
    let signature = b64_decode(request.header("x-tusk-signature"));
    key()
        .verifying_key()
        .verify(
            &request_message("GET", &format!("/v1/repos/{REPO}/blobs"), timestamp),
            &ed25519_dalek::Signature::from_bytes(
                signature.as_slice().try_into().expect("64 bytes"),
            ),
        )
        .expect("request signature verifies");
}

#[test]
fn non_success_status_maps_to_http_error() {
    let (base, _rx) = one_shot_server("403 Forbidden", r#"{"error":"device is not approved"}"#);
    let err = client(&base).append_op(b"x").expect_err("403 append");
    match err {
        tusk_sync::SyncError::Http { status, .. } => assert_eq!(status, 403),
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[test]
fn tampered_op_fails_verification() {
    let sig = key().sign(&op_message(REPO, b"honest-payload"));
    assert!(verify_op(&key().verifying_key(), REPO, b"tampered", &sig.to_bytes()).is_err());
    // Same payload signed for a different repo must not verify either.
    let foreign = key().sign(&op_message(
        "11111111-1111-1111-1111-111111111111",
        b"honest-payload",
    ));
    assert!(verify_op(
        &key().verifying_key(),
        REPO,
        b"honest-payload",
        &foreign.to_bytes()
    )
    .is_err());
}

#[test]
fn presign_put_signs_the_body_and_parses_the_url() {
    use tusk_sync::cloud::request_message_with_body;
    let (base, rx) = one_shot_server(
        "200 OK",
        r#"{"url":"https://nyc3.example.invalid/bkt/repo/a.blob?X-Amz-Signature=x","expires_secs":300}"#,
    );
    let presigned = client(&base)
        .presign_put("a.blob", 1024)
        .expect("presign put");
    assert_eq!(presigned.expires_secs, 300);
    assert!(presigned.url.contains("X-Amz-Signature="));

    let request = rx.recv().expect("captured request");
    assert_eq!(
        request.start_line,
        format!("POST /v1/repos/{REPO}/blobs/presign HTTP/1.1")
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(body["op"], "put");
    assert_eq!(body["name"], "a.blob");
    assert_eq!(body["size_bytes"], 1024);

    // Server-side verification: the signature covers this exact body.
    let timestamp: i64 = request.header("x-tusk-timestamp").parse().expect("ts");
    let signature = b64_decode(request.header("x-tusk-signature"));
    key()
        .verifying_key()
        .verify(
            &request_message_with_body(
                "POST",
                &format!("/v1/repos/{REPO}/blobs/presign"),
                timestamp,
                &request.body,
            ),
            &ed25519_dalek::Signature::from_bytes(
                signature.as_slice().try_into().expect("64 bytes"),
            ),
        )
        .expect("signed-body request signature verifies");
}

#[test]
fn record_blob_posts_the_confirmation() {
    let (base, rx) = one_shot_server("200 OK", r#"{"recorded":"a.blob"}"#);
    client(&base).record_blob("a.blob", 11).expect("record");
    let request = rx.recv().expect("captured request");
    assert_eq!(
        request.start_line,
        format!("POST /v1/repos/{REPO}/blobs HTTP/1.1")
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(body["name"], "a.blob");
    assert_eq!(body["size_bytes"], 11);
}

#[test]
fn quota_rejection_surfaces_as_http_403() {
    let (base, _rx) = one_shot_server("403 Forbidden", r#"{"error":"storage quota exceeded"}"#);
    let err = client(&base)
        .presign_put("big.blob", u64::MAX)
        .expect_err("over quota");
    match err {
        tusk_sync::SyncError::Http { status, .. } => assert_eq!(status, 403),
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[test]
fn c8_fingerprint_matches_pinned_vector() {
    // Pinned in tusk-cloud tests/c4_vectors.rs as well.
    assert_eq!(
        tusk_sync::device_fingerprint(&key().verifying_key().to_bytes()),
        "fe812c12f3ab4ce6"
    );
}

#[test]
fn enroll_posts_keys_and_parses_ids() {
    let (base, rx) = one_shot_server(
        "200 OK",
        r#"{"device_id":"b0e42fe7-31a5-4894-a441-bf1e30cbd7d2","fingerprint":"fe812c12f3ab4ce6","status":"pending"}"#,
    );
    let (device_id, fingerprint) = tusk_sync::enroll_device(
        &base,
        REPO,
        "laptop-b",
        &key().verifying_key().to_bytes(),
        &[9u8; 32],
    )
    .expect("enroll");
    assert_eq!(device_id, DEVICE);
    assert_eq!(fingerprint, "fe812c12f3ab4ce6");

    let request = rx.recv().expect("captured request");
    assert_eq!(
        request.start_line,
        format!("POST /v1/repos/{REPO}/devices/enroll HTTP/1.1")
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["name"], "laptop-b");
    assert_eq!(
        b64_decode(body["ed25519_pubkey"].as_str().expect("key")),
        key().verifying_key().to_bytes()
    );
}

#[test]
fn approve_signs_the_wrap_relay_and_fetch_wrap_round_trips() {
    use tusk_sync::cloud::request_message_with_body;
    let (base, rx) = one_shot_server("200 OK", r#"{"approved":"x"}"#);
    client(&base)
        .approve_device("b0e42fe7-31a5-4894-a441-bf1e30cbd7d2", b"opaque-wrap", 1)
        .expect("approve");
    let request = rx.recv().expect("captured request");
    let path = format!("/v1/repos/{REPO}/devices/b0e42fe7-31a5-4894-a441-bf1e30cbd7d2/approve");
    assert_eq!(request.start_line, format!("POST {path} HTTP/1.1"));
    let timestamp: i64 = request.header("x-tusk-timestamp").parse().expect("ts");
    let signature = b64_decode(request.header("x-tusk-signature"));
    key()
        .verifying_key()
        .verify(
            &request_message_with_body("POST", &path, timestamp, &request.body),
            &ed25519_dalek::Signature::from_bytes(
                signature.as_slice().try_into().expect("64 bytes"),
            ),
        )
        .expect("approve signature verifies");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(
        b64_decode(body["wrap"].as_str().expect("wrap")),
        b"opaque-wrap"
    );

    use base64::Engine;
    let wrap_b64 = base64::engine::general_purpose::STANDARD.encode(b"my-wrap-bytes");
    let response: &'static str =
        Box::leak(format!(r#"{{"wrap":"{wrap_b64}","rmk_generation":3}}"#).into_boxed_str());
    let (base, _rx) = one_shot_server("200 OK", response);
    let (wrap, generation) = client(&base).fetch_wrap().expect("fetch wrap");
    assert_eq!(wrap, b"my-wrap-bytes");
    assert_eq!(generation, 3);
}

// ---------------------------------------------------------------------------
// Account plane (C10 / D29)
// ---------------------------------------------------------------------------

#[test]
fn auth_start_posts_the_email() {
    let (base, rx) = one_shot_server("200 OK", r#"{"sent":true,"expires_secs":600}"#);
    tusk_sync::AccountClient::new(&base, None)
        .expect("client")
        .auth_start("user@example.com")
        .expect("start");
    let request = rx.recv().expect("captured request");
    assert_eq!(request.start_line, "POST /v1/auth/start HTTP/1.1");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["email"], "user@example.com");
}

#[test]
fn auth_verify_returns_the_session_and_arms_the_client() {
    let (base, rx) = one_shot_server(
        "200 OK",
        r#"{"token":"tsks_abc","account_id":"d8df7c5a-e878-43b6-8d04-f4e7b2dddaef","email":"user@example.com","plan":"free"}"#,
    );
    let mut client = tusk_sync::AccountClient::new(&base, None).expect("client");
    let session = client
        .auth_verify("user@example.com", "AAAA-2222", "laptop")
        .expect("verify");
    assert_eq!(session.token, "tsks_abc");
    assert_eq!(session.plan, "free");
    let request = rx.recv().expect("captured request");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["code"], "AAAA-2222");
    assert_eq!(body["label"], "laptop");

    // The client now holds the token: create_repo sends it as a bearer.
    let (base, rx) = one_shot_server(
        "200 OK",
        r#"{"repo_id":"9e86dab9-bb27-4670-ac5b-206e139387ff","device_id":"c2781c22-dac4-4508-b36c-20a84a11b7ee"}"#,
    );
    let client = tusk_sync::AccountClient::new(&base, Some(session.token)).expect("client");
    let (repo_id, device_id) = client
        .create_repo("vault", "laptop", &[1u8; 32], &[2u8; 32])
        .expect("create repo");
    assert_eq!(repo_id, "9e86dab9-bb27-4670-ac5b-206e139387ff");
    assert_eq!(device_id, "c2781c22-dac4-4508-b36c-20a84a11b7ee");
    let request = rx.recv().expect("captured request");
    assert_eq!(request.start_line, "POST /v1/repos HTTP/1.1");
    assert_eq!(request.header("authorization"), "Bearer tsks_abc");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["name"], "vault");
    assert_eq!(
        b64_decode(body["device"]["ed25519_pubkey"].as_str().expect("key")),
        [1u8; 32]
    );
}

#[test]
fn list_repos_sends_the_bearer_and_parses() {
    let (base, rx) = one_shot_server(
        "200 OK",
        r#"[{"repo_id":"9e86dab9-bb27-4670-ac5b-206e139387ff","name":"vault","rmk_generation":2}]"#,
    );
    let client = tusk_sync::AccountClient::new(&base, Some("tsks_abc".into())).expect("client");
    let repos = client.list_repos().expect("list");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, "vault");
    assert_eq!(repos[0].rmk_generation, 2);
    let request = rx.recv().expect("captured request");
    assert_eq!(request.start_line, "GET /v1/repos HTTP/1.1");
    assert_eq!(request.header("authorization"), "Bearer tsks_abc");
}

#[test]
fn expired_session_surfaces_as_http_401() {
    let (base, _rx) = one_shot_server("401 Unauthorized", r#"{"error":"unauthenticated"}"#);
    let client = tusk_sync::AccountClient::new(&base, Some("tsks_dead".into())).expect("client");
    match client.list_repos().expect_err("401") {
        tusk_sync::SyncError::Http { status, .. } => assert_eq!(status, 401),
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[test]
fn delete_repo_sends_the_bearer_delete() {
    let (base, rx) = one_shot_server("200 OK", r#"{"deleted":"x"}"#);
    let client = tusk_sync::AccountClient::new(&base, Some("tsks_abc".into())).expect("client");
    client
        .delete_repo("9e86dab9-bb27-4670-ac5b-206e139387ff")
        .expect("delete");
    let request = rx.recv().expect("captured request");
    assert_eq!(
        request.start_line,
        "DELETE /v1/repos/9e86dab9-bb27-4670-ac5b-206e139387ff HTTP/1.1"
    );
    assert_eq!(request.header("authorization"), "Bearer tsks_abc");
}

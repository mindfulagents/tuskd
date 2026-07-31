//! `CloudClient`: the tusk-cloud `/v1` control-plane client (M1; D23).
//!
//! Mirrors tusk-cloud's C4 signing contract exactly — both repos pin the
//! same golden vectors, so a drift on either side fails a test before it
//! can ship. Two signatures, two jobs:
//!
//! - **Op signature** (durable): ed25519 over
//!   `"tusk-cloud.op.v1" LF repo_id LF hex(sha256(payload))`. The server
//!   stores it and serves it back on pull, so any device can verify op
//!   authorship end-to-end without trusting the server.
//! - **Request signature** (transport, reads): headers `x-tusk-device`,
//!   `x-tusk-timestamp`, `x-tusk-signature` over
//!   `"tusk-cloud.req.v1" LF method LF path LF timestamp`, accepted by the
//!   server within a ±300 s window.
//!
//! Ids (`repo_id`, `device_id`) are server-issued lowercase hyphenated
//! UUIDs; the client treats them as opaque strings but must pass them
//! through verbatim — the repo id is signed into every op message.
//!
//! Blocking HTTP, like [`crate::SpacesProvider`] (same rationale: callers
//! are worker threads, not an async runtime).

use crate::error::SyncError;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Domain-separation prefix for op signatures (C4).
pub const OP_DOMAIN: &str = "tusk-cloud.op.v1";
/// Domain-separation prefix for request signatures (C4).
pub const REQ_DOMAIN: &str = "tusk-cloud.req.v1";

/// The message a device signs for one op (C4).
pub fn op_message(repo_id: &str, payload: &[u8]) -> Vec<u8> {
    let digest = Sha256::digest(payload);
    format!("{OP_DOMAIN}\n{repo_id}\n{}", hex::encode(digest)).into_bytes()
}

/// The message a device signs for one read request (C4).
pub fn request_message(method: &str, path: &str, timestamp: i64) -> Vec<u8> {
    format!("{REQ_DOMAIN}\n{method}\n{path}\n{timestamp}").into_bytes()
}

/// Verify an op signature against a device's verifying key — what a puller
/// runs per op so authorship never rests on trusting the server.
pub fn verify_op(
    key: &VerifyingKey,
    repo_id: &str,
    payload: &[u8],
    signature: &[u8],
) -> Result<(), SyncError> {
    let signature: &[u8; 64] = signature
        .try_into()
        .map_err(|_| SyncError::Crypto("op signature is not 64 bytes".to_string()))?;
    use ed25519_dalek::Verifier;
    key.verify(
        &op_message(repo_id, payload),
        &ed25519_dalek::Signature::from_bytes(signature),
    )
    .map_err(|_| SyncError::Crypto("op signature verification failed".to_string()))
}

/// One sequenced op as served by the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudOp {
    pub seq: i64,
    pub device_id: String,
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Deserialize)]
struct WireOp {
    seq: i64,
    device_id: String,
    payload: String,
    signature: String,
}

#[derive(Deserialize)]
struct PullResponse {
    ops: Vec<WireOp>,
}

#[derive(Deserialize)]
struct AppendResponse {
    seq: i64,
}

/// Blocking client for the tusk-cloud `/v1` device surface. The only
/// credential is the device's ed25519 key — no tokens, no shared secrets.
pub struct CloudClient {
    base_url: String,
    repo_id: String,
    device_id: String,
    key: SigningKey,
    http: reqwest::blocking::Client,
}

impl CloudClient {
    /// `base_url` without a trailing slash, e.g. `https://cloud.opentusk.ai`.
    pub fn new(
        base_url: impl Into<String>,
        repo_id: impl Into<String>,
        device_id: impl Into<String>,
        key: SigningKey,
    ) -> Result<CloudClient, SyncError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| SyncError::Storage(format!("http client: {e}")))?;
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Ok(CloudClient {
            base_url,
            repo_id: repo_id.into(),
            device_id: device_id.into(),
            key,
            http,
        })
    }

    /// Append one opaque (encrypted) op; returns the server-assigned seq.
    pub fn append_op(&self, payload: &[u8]) -> Result<i64, SyncError> {
        let path = format!("/v1/repos/{}/ops", self.repo_id);
        let url = format!("{}{path}", self.base_url);
        let signature = self.key.sign(&op_message(&self.repo_id, payload));
        let body = serde_json::json!({
            "device_id": self.device_id,
            "payload": B64.encode(payload),
            "signature": B64.encode(signature.to_bytes()),
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        let parsed: AppendResponse = resp
            .json()
            .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))?;
        Ok(parsed.seq)
    }

    /// Pull ops with `seq > since`, oldest first. Signatures are returned
    /// raw; run [`verify_op`] against the author device's key.
    pub fn ops_since(&self, since: i64, limit: Option<i64>) -> Result<Vec<CloudOp>, SyncError> {
        let path = format!("/v1/repos/{}/ops", self.repo_id);
        let mut url = format!("{}{path}?since={since}", self.base_url);
        if let Some(limit) = limit {
            url.push_str(&format!("&limit={limit}"));
        }
        let resp = self.signed_get(&url, &path)?;
        let parsed: PullResponse = resp
            .json()
            .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))?;
        parsed
            .ops
            .into_iter()
            .map(|op| {
                Ok(CloudOp {
                    seq: op.seq,
                    device_id: op.device_id,
                    payload: decode_b64(&op.payload)?,
                    signature: decode_b64(&op.signature)?,
                })
            })
            .collect()
    }

    /// Live blob names from the control plane — a bare JSON array (D22;
    /// never S3 XML).
    pub fn list_blobs(&self) -> Result<Vec<String>, SyncError> {
        let path = format!("/v1/repos/{}/blobs", self.repo_id);
        let url = format!("{}{path}", self.base_url);
        let resp = self.signed_get(&url, &path)?;
        resp.json()
            .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))
    }

    /// GET `url` with C4 request-signature headers. `path` is the signed
    /// portion: path only, no query, no host.
    fn signed_get(&self, url: &str, path: &str) -> Result<reqwest::blocking::Response, SyncError> {
        let timestamp = unix_now()?;
        let signature = self.key.sign(&request_message("GET", path, timestamp));
        let resp = self
            .http
            .get(url)
            .header("x-tusk-device", &self.device_id)
            .header("x-tusk-timestamp", timestamp.to_string())
            .header("x-tusk-signature", B64.encode(signature.to_bytes()))
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url: url.to_string(),
            });
        }
        Ok(resp)
    }
}

/// The delta-seconds form of a `Retry-After` header, if present. The
/// HTTP-date form is ignored — tusk-cloud only ever sends seconds.
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn decode_b64(value: &str) -> Result<Vec<u8>, SyncError> {
    B64.decode(value)
        .map_err(|_| SyncError::Storage("invalid base64 in server response".to_string()))
}

fn unix_now() -> Result<i64, SyncError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .map_err(|_| SyncError::Storage("system clock before 1970".to_string()))
}

/// The message a device signs for a request with a body (tusk-cloud C7
/// extension of the C4 scheme): the body hash binds the request content.
pub fn request_message_with_body(method: &str, path: &str, timestamp: i64, body: &[u8]) -> Vec<u8> {
    let digest = Sha256::digest(body);
    format!(
        "{REQ_DOMAIN}\n{method}\n{path}\n{timestamp}\n{}",
        hex::encode(digest)
    )
    .into_bytes()
}

/// A presigned blob URL and its validity window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresignedUrl {
    pub url: String,
    pub expires_secs: u64,
}

#[derive(Deserialize)]
struct WirePresign {
    url: String,
    expires_secs: u64,
}

impl CloudClient {
    /// POST `body` to `path` with C7 signed-body headers.
    fn signed_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::blocking::Response, SyncError> {
        let url = format!("{}{path}", self.base_url);
        let body_bytes = serde_json::to_vec(body)?;
        let timestamp = unix_now()?;
        let signature = self.key.sign(&request_message_with_body(
            "POST",
            path,
            timestamp,
            &body_bytes,
        ));
        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("x-tusk-device", &self.device_id)
            .header("x-tusk-timestamp", timestamp.to_string())
            .header("x-tusk-signature", B64.encode(signature.to_bytes()))
            .body(body_bytes)
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        Ok(resp)
    }

    /// Presign an upload of `size_bytes` (quota is enforced server-side at
    /// issuance; over-quota surfaces as `Http { status: 403 }`).
    pub fn presign_put(&self, name: &str, size_bytes: u64) -> Result<PresignedUrl, SyncError> {
        self.presign(serde_json::json!({
            "op": "put", "name": name, "size_bytes": size_bytes,
        }))
    }

    /// Presign a download of a registered blob (404 if unknown).
    pub fn presign_get(&self, name: &str) -> Result<PresignedUrl, SyncError> {
        self.presign(serde_json::json!({ "op": "get", "name": name }))
    }

    fn presign(&self, body: serde_json::Value) -> Result<PresignedUrl, SyncError> {
        let path = format!("/v1/repos/{}/blobs/presign", self.repo_id);
        let url = format!("{}{path}", self.base_url);
        let parsed: WirePresign = self
            .signed_post(&path, &body)?
            .json()
            .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))?;
        Ok(PresignedUrl {
            url: parsed.url,
            expires_secs: parsed.expires_secs,
        })
    }

    /// Tombstone a blob in the registry: it leaves listings and stops
    /// counting toward quota (C7 addendum; the stored object is reconciled
    /// later server-side). Signed like a read, over DELETE + path.
    pub fn delete_blob(&self, name: &str) -> Result<(), SyncError> {
        let path = format!("/v1/repos/{}/blobs/{name}", self.repo_id);
        let url = format!("{}{path}", self.base_url);
        let timestamp = unix_now()?;
        let signature = self.key.sign(&request_message("DELETE", &path, timestamp));
        let resp = self
            .http
            .delete(&url)
            .header("x-tusk-device", &self.device_id)
            .header("x-tusk-timestamp", timestamp.to_string())
            .header("x-tusk-signature", B64.encode(signature.to_bytes()))
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        Ok(())
    }

    /// Confirm an upload into the control plane's blob registry so it
    /// appears in listings and counts toward quota.
    pub fn record_blob(&self, name: &str, size_bytes: u64) -> Result<(), SyncError> {
        let path = format!("/v1/repos/{}/blobs", self.repo_id);
        self.signed_post(
            &path,
            &serde_json::json!({ "name": name, "size_bytes": size_bytes }),
        )?;
        Ok(())
    }
}

/// `StorageProvider` over the tusk-cloud control plane (D24): puts and gets
/// flow through short-lived presigned URLs (the client never holds bucket
/// credentials), the listing is the control-plane JSON array (D22), and
/// delete is the registry tombstone. Composes directly with the M0 crypto
/// pipeline — see the `m0_roundtrip` push/pull shape.
pub struct CloudProvider {
    client: CloudClient,
    http: reqwest::blocking::Client,
}

impl CloudProvider {
    pub fn new(client: CloudClient) -> Result<CloudProvider, SyncError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| SyncError::Storage(format!("http client: {e}")))?;
        Ok(CloudProvider { client, http })
    }
}

impl crate::provider::StorageProvider for CloudProvider {
    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError> {
        crate::provider::validate_name(name)?;
        let presigned = self.client.presign_put(name, bytes.len() as u64)?;
        let resp = self
            .http
            .put(&presigned.url)
            .body(bytes.to_vec())
            .send()
            .map_err(|e| SyncError::Storage(format!("blob PUT {name}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url: presigned.url,
            });
        }
        self.client.record_blob(name, bytes.len() as u64)
    }

    fn get(&self, name: &str) -> Result<Vec<u8>, SyncError> {
        crate::provider::validate_name(name)?;
        let presigned = match self.client.presign_get(name) {
            Err(SyncError::Http { status: 404, .. }) => {
                return Err(SyncError::NotFound(name.to_string()))
            }
            other => other?,
        };
        let resp = self
            .http
            .get(&presigned.url)
            .send()
            .map_err(|e| SyncError::Storage(format!("blob GET {name}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url: presigned.url,
            });
        }
        resp.bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|e| SyncError::Storage(format!("blob GET {name} body: {e}")))
    }

    fn delete(&self, name: &str) -> Result<(), SyncError> {
        crate::provider::validate_name(name)?;
        self.client.delete_blob(name)
    }

    fn list(&self) -> Result<Vec<String>, SyncError> {
        self.client.list_blobs()
    }
}

/// Human-checkable device fingerprint (tusk-cloud C8): first 8 bytes of
/// sha256(ed25519 pubkey), hex — 16 chars both ends render identically
/// for the out-of-band approval check.
pub fn device_fingerprint(ed25519_pubkey: &[u8]) -> String {
    hex::encode(&Sha256::digest(ed25519_pubkey)[..8])
}

/// A device as listed by the control plane (public material only).
#[derive(Debug, Clone, Deserialize)]
pub struct WireDevice {
    pub device_id: String,
    pub name: String,
    pub status: String,
    pub ed25519_pubkey: String,
    pub x25519_pubkey: String,
    pub fingerprint: String,
}

#[derive(Deserialize)]
struct EnrollResponse {
    device_id: String,
    fingerprint: String,
}

#[derive(Deserialize)]
struct WrapResponse {
    wrap: String,
    rmk_generation: i32,
}

/// Enroll a new (pending) device — the pre-client step: it happens before
/// a `CloudClient` exists because the server has not issued a device id
/// yet. Returns `(device_id, fingerprint)`; the caller should display the
/// fingerprint and confirm it matches what the approving side sees.
pub fn enroll_device(
    base_url: &str,
    repo_id: &str,
    name: &str,
    ed25519_pubkey: &[u8; 32],
    x25519_pubkey: &[u8; 32],
) -> Result<(String, String), SyncError> {
    let base_url = base_url.trim_end_matches('/');
    let url = format!("{base_url}/v1/repos/{repo_id}/devices/enroll");
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| SyncError::Storage(format!("http client: {e}")))?;
    let resp = http
        .post(&url)
        .json(&serde_json::json!({
            "name": name,
            "ed25519_pubkey": B64.encode(ed25519_pubkey),
            "x25519_pubkey": B64.encode(x25519_pubkey),
        }))
        .send()
        .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(SyncError::Http {
            status: resp.status().as_u16(),
            url,
        });
    }
    let parsed: EnrollResponse = resp
        .json()
        .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))?;
    Ok((parsed.device_id, parsed.fingerprint))
}

impl CloudClient {
    /// All devices of the repo, for the approval UI/CLI.
    pub fn list_devices(&self) -> Result<Vec<WireDevice>, SyncError> {
        let path = format!("/v1/repos/{}/devices", self.repo_id);
        let url = format!("{}{path}", self.base_url);
        let resp = self.signed_get(&url, &path)?;
        resp.json()
            .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))
    }

    /// Approve a pending device, relaying the opaque RMK wrap produced
    /// on this device (`wrap_rmk_for_device`, serialized).
    pub fn approve_device(
        &self,
        device_id: &str,
        wrap: &[u8],
        rmk_generation: i32,
    ) -> Result<(), SyncError> {
        let path = format!("/v1/repos/{}/devices/{device_id}/approve", self.repo_id);
        self.signed_post(
            &path,
            &serde_json::json!({
                "wrap": B64.encode(wrap),
                "rmk_generation": rmk_generation,
            }),
        )?;
        Ok(())
    }

    /// This device's own latest-generation RMK wrap. 403 until approved
    /// (the natural "am I approved yet" poll), 404 if approved without a
    /// wrap on record.
    pub fn fetch_wrap(&self) -> Result<(Vec<u8>, i32), SyncError> {
        let path = format!("/v1/repos/{}/wrap", self.repo_id);
        let url = format!("{}{path}", self.base_url);
        let resp = self.signed_get(&url, &path)?;
        let parsed: WrapResponse = resp
            .json()
            .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))?;
        Ok((decode_b64(&parsed.wrap)?, parsed.rmk_generation))
    }
}

#[derive(Deserialize)]
struct RevokeResponse {
    rmk_generation: i32,
}

impl CloudClient {
    /// Revoke another device (tusk-cloud C9). Returns the new RMK
    /// generation; the caller must then rotate: generate a new RMK,
    /// re-seal the key table, and re-issue wraps at that generation.
    pub fn revoke_device(&self, device_id: &str) -> Result<i32, SyncError> {
        let path = format!("/v1/repos/{}/devices/{device_id}/revoke", self.repo_id);
        let url = format!("{}{path}", self.base_url);
        let timestamp = unix_now()?;
        let signature = self.key.sign(&request_message("POST", &path, timestamp));
        let resp = self
            .http
            .post(&url)
            .header("x-tusk-device", &self.device_id)
            .header("x-tusk-timestamp", timestamp.to_string())
            .header("x-tusk-signature", B64.encode(signature.to_bytes()))
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        let parsed: RevokeResponse = resp
            .json()
            .map_err(|e| SyncError::Storage(format!("response from {url}: {e}")))?;
        Ok(parsed.rmk_generation)
    }

    /// Push a rotation re-wrap for a remaining approved device (C9).
    pub fn push_wrap(
        &self,
        device_id: &str,
        wrap: &[u8],
        rmk_generation: i32,
    ) -> Result<(), SyncError> {
        let path = format!("/v1/repos/{}/devices/{device_id}/wrap", self.repo_id);
        self.signed_post(
            &path,
            &serde_json::json!({
                "wrap": B64.encode(wrap),
                "rmk_generation": rmk_generation,
            }),
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Account plane (tusk-cloud C10; tuskd D29)
// ---------------------------------------------------------------------------

/// A verified sign-in: the bearer session token plus account facts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AccountSession {
    pub token: String,
    pub account_id: String,
    pub email: String,
    pub plan: String,
}

/// One repo as listed to its owning account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AccountRepo {
    pub repo_id: String,
    pub name: String,
    pub rmk_generation: i32,
}

#[derive(Deserialize)]
struct CreateRepoResponse {
    repo_id: String,
    device_id: String,
}

/// Client for the session-authed account plane (C10): emailed sign-in
/// codes, sessions, and repo management. Distinct from [`CloudClient`] on
/// purpose — that is the device plane (ed25519, per-repo); this side is a
/// plain bearer token for a person's account.
pub struct AccountClient {
    base_url: String,
    token: Option<String>,
    http: reqwest::blocking::Client,
}

impl AccountClient {
    pub fn new(
        base_url: impl Into<String>,
        token: Option<String>,
    ) -> Result<AccountClient, SyncError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| SyncError::Storage(format!("http client: {e}")))?;
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Ok(AccountClient {
            base_url,
            token,
            http,
        })
    }

    fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
        authed: bool,
    ) -> Result<reqwest::blocking::Response, SyncError> {
        let url = format!("{}{path}", self.base_url);
        let mut request = self.http.post(&url).json(body);
        if authed {
            let token = self
                .token
                .as_deref()
                .ok_or_else(|| SyncError::Storage("not logged in".into()))?;
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if resp.status().as_u16() == 429 {
            return Err(SyncError::RateLimited {
                retry_after_secs: retry_after_secs(resp.headers()),
                url,
            });
        }
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        Ok(resp)
    }

    /// Request a sign-in code for `email`.
    pub fn auth_start(&self, email: &str) -> Result<(), SyncError> {
        self.post(
            "/v1/auth/start",
            &serde_json::json!({ "email": email }),
            false,
        )?;
        Ok(())
    }

    /// Exchange the emailed code for a session.
    pub fn auth_verify(
        &mut self,
        email: &str,
        code: &str,
        label: &str,
    ) -> Result<AccountSession, SyncError> {
        let session: AccountSession = self
            .post(
                "/v1/auth/verify",
                &serde_json::json!({ "email": email, "code": code, "label": label }),
                false,
            )?
            .json()
            .map_err(|e| SyncError::Storage(format!("verify response: {e}")))?;
        self.token = Some(session.token.clone());
        Ok(session)
    }

    /// Create a repo, registering this device as its first approved device.
    /// Returns `(repo_id, device_id)`.
    pub fn create_repo(
        &self,
        name: &str,
        device_name: &str,
        ed25519_pubkey: &[u8; 32],
        x25519_pubkey: &[u8; 32],
    ) -> Result<(String, String), SyncError> {
        let created: CreateRepoResponse = self
            .post(
                "/v1/repos",
                &serde_json::json!({
                    "name": name,
                    "device": {
                        "name": device_name,
                        "ed25519_pubkey": B64.encode(ed25519_pubkey),
                        "x25519_pubkey": B64.encode(x25519_pubkey),
                    }
                }),
                true,
            )?
            .json()
            .map_err(|e| SyncError::Storage(format!("create-repo response: {e}")))?;
        Ok((created.repo_id, created.device_id))
    }

    /// Permanently delete an owned repo (tusk-cloud C12).
    pub fn delete_repo(&self, repo_id: &str) -> Result<(), SyncError> {
        let path = format!("/v1/repos/{repo_id}");
        let url = format!("{}{path}", self.base_url);
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| SyncError::Storage("not logged in".into()))?;
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(token)
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        Ok(())
    }

    /// Rename an owned repo (tusk-cloud C15) — display metadata only,
    /// the repo id never changes.
    pub fn rename_repo(&self, repo_id: &str, name: &str) -> Result<(), SyncError> {
        let path = format!("/v1/repos/{repo_id}");
        let url = format!("{}{path}", self.base_url);
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| SyncError::Storage("not logged in".into()))?;
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        Ok(())
    }

    /// The account's repos.
    pub fn list_repos(&self) -> Result<Vec<AccountRepo>, SyncError> {
        let path = "/v1/repos";
        let url = format!("{}{path}", self.base_url);
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| SyncError::Storage("not logged in".into()))?;
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SyncError::Http {
                status: resp.status().as_u16(),
                url,
            });
        }
        resp.json()
            .map_err(|e| SyncError::Storage(format!("repos response: {e}")))
    }
}

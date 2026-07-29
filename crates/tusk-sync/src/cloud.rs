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

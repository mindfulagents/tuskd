//! `SpacesProvider`: `StorageProvider` over presigned URLs.
//!
//! This provider holds **no credentials and signs nothing**. In M1,
//! tusk-cloud issues short-lived presigned DO Spaces URLs (it is the only
//! party with bucket keys); the client asks its [`PresignSource`] for a URL
//! per operation and performs the bare HTTP verb. The same shape works
//! against any S3-compatible store — and against the plain test server in
//! this repo's tests.

use crate::error::SyncError;
use crate::provider::{validate_name, StorageProvider};
use std::time::Duration;

/// The storage operation a presigned URL is requested for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOp {
    Put,
    Get,
    Delete,
    List,
}

/// Where presigned URLs come from. In M1 this is a tusk-cloud API client
/// (which also enforces quotas server-side); in tests, string concatenation
/// onto a local server. `name` is the blob name (empty for `List`).
pub trait PresignSource: Send + Sync {
    fn url_for(&self, op: BlobOp, name: &str) -> Result<String, SyncError>;
}

/// Blocking HTTP blob store over presigned URLs (see module docs and the
/// sync-vs-async rationale on [`StorageProvider`]).
///
/// `list` expects the list URL to return a JSON array of blob names —
/// listing is a control-plane call answered by tusk-cloud (which tracks
/// blobs in its oplog), not a raw S3 `ListObjectsV2`; the client
/// deliberately never parses S3 XML.
pub struct SpacesProvider<S: PresignSource> {
    source: S,
    client: reqwest::blocking::Client,
}

impl<S: PresignSource> SpacesProvider<S> {
    pub fn new(source: S) -> Result<SpacesProvider<S>, SyncError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| SyncError::Storage(format!("http client: {e}")))?;
        Ok(SpacesProvider { source, client })
    }

    fn send(
        &self,
        req: reqwest::blocking::RequestBuilder,
        url: &str,
    ) -> Result<reqwest::blocking::Response, SyncError> {
        req.send()
            .map_err(|e| SyncError::Storage(format!("request to {url}: {e}")))
    }
}

fn status_err(resp: &reqwest::blocking::Response, url: &str) -> SyncError {
    SyncError::Http {
        status: resp.status().as_u16(),
        url: url.to_string(),
    }
}

impl<S: PresignSource> StorageProvider for SpacesProvider<S> {
    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError> {
        validate_name(name)?;
        let url = self.source.url_for(BlobOp::Put, name)?;
        let resp = self.send(self.client.put(&url).body(bytes.to_vec()), &url)?;
        if !resp.status().is_success() {
            return Err(status_err(&resp, &url));
        }
        Ok(())
    }

    fn get(&self, name: &str) -> Result<Vec<u8>, SyncError> {
        validate_name(name)?;
        let url = self.source.url_for(BlobOp::Get, name)?;
        let resp = self.send(self.client.get(&url), &url)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SyncError::NotFound(name.to_string()));
        }
        if !resp.status().is_success() {
            return Err(status_err(&resp, &url));
        }
        let bytes = resp
            .bytes()
            .map_err(|e| SyncError::Storage(format!("body from {url}: {e}")))?;
        Ok(bytes.to_vec())
    }

    fn delete(&self, name: &str) -> Result<(), SyncError> {
        validate_name(name)?;
        let url = self.source.url_for(BlobOp::Delete, name)?;
        let resp = self.send(self.client.delete(&url), &url)?;
        // S3/Spaces DeleteObject is idempotent (204 even when absent); treat
        // an explicit 404 from other backends the same way.
        if resp.status() == reqwest::StatusCode::NOT_FOUND || resp.status().is_success() {
            return Ok(());
        }
        Err(status_err(&resp, &url))
    }

    fn list(&self) -> Result<Vec<String>, SyncError> {
        let url = self.source.url_for(BlobOp::List, "")?;
        let resp = self.send(self.client.get(&url), &url)?;
        if !resp.status().is_success() {
            return Err(status_err(&resp, &url));
        }
        let bytes = resp
            .bytes()
            .map_err(|e| SyncError::Storage(format!("body from {url}: {e}")))?;
        let mut names: Vec<String> = serde_json::from_slice(&bytes)?;
        names.sort();
        Ok(names)
    }
}

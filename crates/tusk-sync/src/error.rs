//! tusk-sync error type (thiserror, per repo ground rules).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("io {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Blob absent from the provider (LocalProvider missing file, HTTP 404).
    #[error("blob not found: {0}")]
    NotFound(String),

    /// Blob names are provider keys, not paths; anything outside the
    /// conservative charset is rejected before it touches storage.
    #[error("invalid blob name {0:?}")]
    InvalidName(String),

    /// AEAD/KDF/key-conversion failure — deliberately unstructured so error
    /// text can never leak key material or plaintext.
    #[error("crypto: {0}")]
    Crypto(String),

    /// Repo Secret Key parse failure (mnemonic or `otsk_` form).
    #[error("secret key: {0}")]
    SecretKey(String),

    /// Provider transport failure that is not an HTTP status (connect,
    /// timeout, body read).
    #[error("storage: {0}")]
    Storage(String),

    #[error("http {status} for {url}")]
    Http { status: u16, url: String },

    /// HTTP 429 — the server refused to send another sign-in code (or
    /// otherwise throttled the account plane). `retry_after_secs` is the
    /// server's `Retry-After` header when it sends one.
    #[error("rate limited by {url}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        url: String,
    },

    #[error("serialize: {0}")]
    Serde(#[from] serde_json::Error),
}

impl SyncError {
    pub fn io(path: impl std::fmt::Display, source: std::io::Error) -> Self {
        SyncError::Io {
            path: path.to_string(),
            source,
        }
    }
}

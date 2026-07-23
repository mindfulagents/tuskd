use thiserror::Error;

/// Unified error type for tusk-core operations.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("FTS5 unavailable in bundled SQLite: {0}")]
    FtsUnavailable(String),

    #[error("frontmatter parse error: {0}")]
    Frontmatter(String),

    #[error("invalid scope: {0}")]
    InvalidScope(String),

    #[error("record not found: {0}")]
    NotFound(String),

    #[error("denied: {0}")]
    Denied(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("vault is locked by another process: {0}")]
    Locked(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl CoreError {
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        CoreError::Io {
            path: path.into(),
            source,
        }
    }
}

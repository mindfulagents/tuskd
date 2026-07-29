#![forbid(unsafe_code)]

//! tusk-sync: client-side storage abstraction and crypto for hot-cache sync
//! (M0 slice, HOT_CACHE_SYNC_PROPOSAL §4 + §6 items 3–4; DECISIONS D22).
//!
//! Everything here runs on the client; the server side (tusk-cloud) only ever
//! sees the opaque blobs these modules produce. No daemon wiring in this
//! slice — the sync worker and admin verbs land in M1.

pub mod cloud;
pub mod crypto;
pub mod error;
pub mod provider;
pub mod spaces;
pub mod wrap;

pub use cloud::{verify_op, CloudClient, CloudOp};
pub use crypto::{Dek, KeyTable, ObjectEntry, RepoMasterKey};
pub use error::SyncError;
pub use provider::{LocalProvider, StorageProvider};
pub use spaces::{BlobOp, PresignSource, SpacesProvider};
pub use wrap::DeviceWrap;

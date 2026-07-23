#![forbid(unsafe_code)]

//! tusk-core: vault, indexer, keyring, gate, config — the tuskd kernel.

pub mod clock;
pub mod error;
pub mod frontmatter;
pub mod fts;
pub mod record;
pub mod scope;
pub mod vault;

pub use clock::{Clock, FakeClock, SystemClock};
pub use error::CoreError;

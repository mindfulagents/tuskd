#![forbid(unsafe_code)]

//! tusk-core: vault, indexer, keyring, gate, config — the tuskd kernel.

pub mod clock;
pub mod error;
pub mod frontmatter;
pub mod fts;
pub mod gate;
pub mod index;
pub mod keyring;
pub mod record;
pub mod scope;
pub mod sync;
pub mod vault;
pub mod watch;

pub use clock::{Clock, FakeClock, SystemClock};
pub use error::CoreError;

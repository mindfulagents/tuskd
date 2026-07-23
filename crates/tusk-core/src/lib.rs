#![forbid(unsafe_code)]

//! tusk-core: vault, indexer, keyring, gate, config — the tuskd kernel.

pub mod clock;
pub mod error;
pub mod fts;

pub use clock::{Clock, FakeClock, SystemClock};
pub use error::CoreError;

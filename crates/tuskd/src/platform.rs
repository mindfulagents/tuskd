//! Platform-specific defaults live here and only here (build-loop §0).
//! On Windows later: named pipe behind the same seam.

use std::path::{Path, PathBuf};

/// Default Unix-domain-socket path for a vault.
pub fn default_uds_path(vault: &Path) -> PathBuf {
    vault.join(".tusk").join("tuskd.sock")
}

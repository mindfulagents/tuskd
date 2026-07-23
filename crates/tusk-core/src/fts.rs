use crate::error::CoreError;
use rusqlite::Connection;

/// Verify the bundled SQLite has FTS5. Called at every boot; failing loudly is
/// required, silently degrading is forbidden (spec §2.3, build-loop §3.6).
pub fn verify_fts5() -> Result<(), CoreError> {
    let conn = Connection::open_in_memory()?;
    probe_fts5(&conn)
}

/// Probe an open connection for FTS5 by creating a scratch virtual table.
pub fn probe_fts5(conn: &Connection) -> Result<(), CoreError> {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE temp.__fts5_probe USING fts5(x);
         DROP TABLE temp.__fts5_probe;",
    )
    .map_err(|e| CoreError::FtsUnavailable(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Permanent boot-probe test — must never be removed (build-loop §1 exit).
    #[test]
    fn fts5_available_in_bundled_sqlite() {
        verify_fts5().expect("bundled SQLite must include FTS5");
    }
}

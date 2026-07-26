//! Derived SQLite index — always rebuildable from the vault (spec §3.3).

use crate::error::CoreError;
use crate::fts::probe_fts5;
use crate::record::{Record, RecordType};
use crate::scope::Scope;
use crate::vault::VaultStore;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

/// Ranking constants (spec §3.3): `score = -bm25 + success_rate
/// + uses_weight·ln(1+uses) + trust_weight·trust`.
#[derive(Debug, Clone, Copy)]
pub struct RankingConfig {
    pub uses_weight: f64,
    pub trust_weight: f64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig {
            uses_weight: 0.3,
            trust_weight: 0.2,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: String,
    pub scopes: Option<Vec<Scope>>,
    pub kind: Option<RecordType>,
    pub tags: Option<Vec<String>>,
    pub as_of: Option<DateTime<Utc>>,
    pub include_invalid: bool,
    /// 0 = default (10). Callers cap at 50 (spec §5).
    pub k: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub scope: String,
    pub author: String,
    pub created_at: String,
    pub valid_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    pub entities: Vec<String>,
    pub tags: Vec<String>,
    pub trust: f64,
    pub uses: i64,
    pub successes: f64,
    pub success_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    pub score: f64,
    pub snippet: String,
    #[serde(skip)]
    pub path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexStats {
    pub total: i64,
    pub valid: i64,
    pub by_scope: Vec<(String, i64)>,
    pub by_type: Vec<(String, i64)>,
}

/// Per-author aggregate for the dashboard overview.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthorActivity {
    pub author: String,
    pub records: i64,
    pub valid: i64,
    pub last_created: String,
}

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS records (
    id          TEXT PRIMARY KEY,
    path        TEXT NOT NULL,
    type        TEXT NOT NULL,
    scope       TEXT NOT NULL,
    author      TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    valid_at    TEXT NOT NULL,
    invalid_at  TEXT,
    supersedes  TEXT,
    entities    TEXT NOT NULL,
    tags        TEXT NOT NULL,
    trust       REAL NOT NULL,
    uses        INTEGER NOT NULL,
    successes   REAL NOT NULL,
    last_used   TEXT,
    version     INTEGER,
    trigger     TEXT,
    body_hash   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_records_scope ON records(scope);
CREATE INDEX IF NOT EXISTS idx_records_body_hash ON records(body_hash);
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(id UNINDEXED, body, entities, tags);
";

/// Single-owner SQLite index. WAL + busy_timeout as defense in depth
/// (spec §2.1); FTS5 probed at open (build-loop §3.6).
pub struct Indexer {
    conn: Mutex<Connection>,
    ranking: RankingConfig,
}

impl Indexer {
    pub fn open(db_path: &Path, ranking: RankingConfig) -> Result<Indexer, CoreError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::io(parent.display().to_string(), e))?;
        }
        let conn = Connection::open(db_path)?;
        Self::from_connection(conn, ranking)
    }

    pub fn open_in_memory(ranking: RankingConfig) -> Result<Indexer, CoreError> {
        Self::from_connection(Connection::open_in_memory()?, ranking)
    }

    fn from_connection(conn: Connection, ranking: RankingConfig) -> Result<Indexer, CoreError> {
        probe_fts5(&conn)?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Indexer {
            conn: Mutex::new(conn),
            ranking,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, CoreError> {
        self.conn
            .lock()
            .map_err(|_| CoreError::Other("index connection poisoned".into()))
    }

    pub fn ingest_record(&self, path: &Path, rec: &Record) -> Result<(), CoreError> {
        let conn = self.lock()?;
        ingest_with(&conn, path, rec)
    }

    pub fn remove(&self, id: &str) -> Result<(), CoreError> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM records WHERE id = ?1", params![id])?;
        conn.execute("DELETE FROM fts WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Wipe and re-walk the vault. Idempotent.
    pub fn rebuild(&self, vault: &VaultStore) -> Result<usize, CoreError> {
        let records = vault.walk()?;
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM records", [])?;
        tx.execute("DELETE FROM fts", [])?;
        for (path, rec) in &records {
            ingest_with(&tx, path, rec)?;
        }
        tx.commit()?;
        Ok(records.len())
    }

    /// Re-index one path after a filesystem event. Missing file ⇒ remove by
    /// id (filename stem); unparseable file ⇒ skip, retried on a later event
    /// (build-loop §3.4).
    pub fn refresh_path(&self, vault: &VaultStore, path: &Path) -> Result<(), CoreError> {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.ends_with(".md") && !n.starts_with('.') => n.to_string(),
            _ => return Ok(()),
        };
        let id = name.trim_end_matches(".md").to_string();
        if !path.exists() {
            return self.remove(&id);
        }
        match vault.load(path) {
            Ok(rec) => self.ingest_record(path, &rec),
            Err(_) => Ok(()), // partial write; tolerate
        }
    }

    pub fn scopes_present(&self) -> Result<Vec<String>, CoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT DISTINCT scope FROM records")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// One indexed query for exact dedup (build-loop §3.5): valid records in
    /// `scope` whose trimmed-body sha256 equals `hash`.
    pub fn find_valid_by_body_hash(
        &self,
        scope: &Scope,
        hash: &str,
    ) -> Result<Vec<String>, CoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM records WHERE scope = ?1 AND body_hash = ?2 AND invalid_at IS NULL",
        )?;
        let rows = stmt.query_map(params![scope.to_string(), hash], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn stats(&self) -> Result<IndexStats, CoreError> {
        let conn = self.lock()?;
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
        let valid: i64 = conn.query_row(
            "SELECT COUNT(*) FROM records WHERE invalid_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        let mut by_scope = Vec::new();
        let mut stmt =
            conn.prepare("SELECT scope, COUNT(*) FROM records GROUP BY scope ORDER BY scope")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for r in rows {
            by_scope.push(r?);
        }
        let mut by_type = Vec::new();
        let mut stmt =
            conn.prepare("SELECT type, COUNT(*) FROM records GROUP BY type ORDER BY type")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for r in rows {
            by_type.push(r?);
        }
        Ok(IndexStats {
            total,
            valid,
            by_scope,
            by_type,
        })
    }

    /// Records created per UTC day since `since` (inclusive), ascending by
    /// day. Days with no records are absent — callers zero-fill.
    pub fn created_per_day(&self, since: DateTime<Utc>) -> Result<Vec<(String, i64)>, CoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT substr(created_at, 1, 10) AS day, COUNT(*) FROM records \
             WHERE created_at >= ?1 GROUP BY day ORDER BY day",
        )?;
        let since_ts = fmt_ts(crate::vault::trunc_ms(since));
        let rows = stmt.query_map(params![since_ts], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Per-author record counts and latest creation, most-active first.
    pub fn author_activity(&self) -> Result<Vec<AuthorActivity>, CoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT author, COUNT(*), \
             SUM(CASE WHEN invalid_at IS NULL THEN 1 ELSE 0 END), MAX(created_at) \
             FROM records GROUP BY author ORDER BY COUNT(*) DESC, author",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AuthorActivity {
                author: r.get(0)?,
                records: r.get(1)?,
                valid: r.get(2)?,
                last_created: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, CoreError> {
        let k = if query.k == 0 { 10 } else { query.k };
        let terms: Vec<String> = query
            .query
            .split_whitespace()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();

        let mut sql = String::from(
            "SELECT r.id, r.path, r.type, r.scope, r.author, r.created_at, r.valid_at, \
             r.invalid_at, r.supersedes, r.entities, r.tags, r.trust, r.uses, r.successes, \
             r.version, r.trigger, f.body",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if terms.is_empty() {
            sql.push_str(", 0.0 AS rank FROM records r JOIN fts f ON f.id = r.id WHERE 1=1");
        } else {
            sql.push_str(
                ", bm25(fts) AS rank FROM fts f JOIN records r ON r.id = f.id WHERE fts MATCH ?",
            );
            params_vec.push(Box::new(terms.join(" OR ")));
        }

        match query.as_of {
            Some(t) => {
                let ts = fmt_ts(crate::vault::trunc_ms(t));
                sql.push_str(" AND r.valid_at <= ? AND (r.invalid_at IS NULL OR r.invalid_at > ?)");
                params_vec.push(Box::new(ts.clone()));
                params_vec.push(Box::new(ts));
            }
            None => {
                if !query.include_invalid {
                    sql.push_str(" AND r.invalid_at IS NULL");
                }
            }
        }
        if let Some(kind) = query.kind {
            sql.push_str(" AND r.type = ?");
            params_vec.push(Box::new(kind.to_string()));
        }
        if let Some(scopes) = &query.scopes {
            if scopes.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(" AND r.scope IN (");
            for (i, s) in scopes.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push('?');
                params_vec.push(Box::new(s.to_string()));
            }
            sql.push(')');
        }
        if terms.is_empty() {
            sql.push_str(" ORDER BY r.created_at DESC, r.id DESC LIMIT 500");
        } else {
            sql.push_str(" ORDER BY rank LIMIT 500");
        }

        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |r| {
            Ok(RawRow {
                id: r.get(0)?,
                path: r.get(1)?,
                kind: r.get(2)?,
                scope: r.get(3)?,
                author: r.get(4)?,
                created_at: r.get(5)?,
                valid_at: r.get(6)?,
                invalid_at: r.get(7)?,
                supersedes: r.get(8)?,
                entities: r.get(9)?,
                tags: r.get(10)?,
                trust: r.get(11)?,
                uses: r.get(12)?,
                successes: r.get(13)?,
                version: r.get(14)?,
                trigger: r.get(15)?,
                body: r.get(16)?,
                rank: r.get(17)?,
            })
        })?;

        let ranked = !terms.is_empty();
        let mut hits: Vec<SearchHit> = Vec::new();
        for row in rows {
            let row = row?;
            let tags: Vec<String> = serde_json::from_str(&row.tags).unwrap_or_default();
            if let Some(want) = &query.tags {
                if !want.is_empty() && !want.iter().all(|t| tags.contains(t)) {
                    continue;
                }
            }
            let entities: Vec<String> = serde_json::from_str(&row.entities).unwrap_or_default();
            let success_rate = if row.uses > 0 {
                row.successes / row.uses as f64
            } else {
                0.0
            };
            let score = if ranked {
                -row.rank
                    + success_rate
                    + self.ranking.uses_weight * (1.0 + row.uses as f64).ln()
                    + self.ranking.trust_weight * row.trust
            } else {
                0.0
            };
            let snippet: String = row.body.trim().chars().take(240).collect();
            hits.push(SearchHit {
                id: row.id,
                kind: row.kind,
                scope: row.scope,
                author: row.author,
                created_at: row.created_at,
                valid_at: row.valid_at,
                invalid_at: row.invalid_at,
                supersedes: row.supersedes,
                entities,
                tags,
                trust: row.trust,
                uses: row.uses,
                successes: row.successes,
                success_rate,
                trigger: row.trigger,
                version: row.version,
                score,
                snippet,
                path: row.path,
            });
        }
        if ranked {
            hits.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        hits.truncate(k);
        Ok(hits)
    }
}

struct RawRow {
    id: String,
    path: String,
    kind: String,
    scope: String,
    author: String,
    created_at: String,
    valid_at: String,
    invalid_at: Option<String>,
    supersedes: Option<String>,
    entities: String,
    tags: String,
    trust: f64,
    uses: i64,
    successes: f64,
    version: Option<i64>,
    trigger: Option<String>,
    body: String,
    rank: f64,
}

fn ingest_with(conn: &Connection, path: &Path, rec: &Record) -> Result<(), CoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO records
         (id, path, type, scope, author, created_at, valid_at, invalid_at, supersedes,
          entities, tags, trust, uses, successes, last_used, version, trigger, body_hash)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![
            rec.id,
            path.display().to_string(),
            rec.kind.to_string(),
            rec.scope.to_string(),
            rec.author,
            fmt_ts(rec.created_at),
            fmt_ts(rec.valid_at),
            rec.invalid_at.map(fmt_ts),
            rec.supersedes,
            serde_json::to_string(&rec.entities)?,
            serde_json::to_string(&rec.tags)?,
            rec.trust,
            rec.uses,
            rec.successes,
            rec.last_used.map(fmt_ts),
            rec.version,
            rec.trigger,
            rec.body_hash(),
        ],
    )?;
    conn.execute("DELETE FROM fts WHERE id = ?1", params![rec.id])?;
    conn.execute(
        "INSERT INTO fts (id, body, entities, tags) VALUES (?1,?2,?3,?4)",
        params![rec.id, rec.body, rec.entities.join(" "), rec.tags.join(" ")],
    )?;
    Ok(())
}

use crate::clock::Clock;
use crate::error::CoreError;
use crate::record::{Record, RecordType};
use crate::scope::Scope;
use crate::sync::{self, Journal, OpKind};
use chrono::{DateTime, TimeZone, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Truncate to millisecond precision — the on-disk timestamp resolution.
pub fn trunc_ms(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(t.timestamp_millis())
        .single()
        .unwrap_or(t)
}

/// Files are the source of truth; VaultStore owns all reads/writes of
/// `vault/memory/**` record files.
pub struct VaultStore {
    root: PathBuf,
    clock: Arc<dyn Clock>,
    /// When sync is enabled (D20), every mutation below also appends to the
    /// hash-chained change journal. None (the default) = behavior unchanged.
    journal: Mutex<Option<Arc<Journal>>>,
}

impl VaultStore {
    /// Open a vault root, creating the standard layout if absent.
    pub fn init(root: &Path, clock: Arc<dyn Clock>) -> Result<VaultStore, CoreError> {
        for sub in ["memory", "skills", ".tusk"] {
            let dir = root.join(sub);
            fs::create_dir_all(&dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        }
        // Canonicalize so watcher event paths (which the OS canonicalizes,
        // e.g. /var -> /private/var on macOS) compare against index paths.
        let root =
            fs::canonicalize(root).map_err(|e| CoreError::io(root.display().to_string(), e))?;
        Ok(VaultStore {
            root,
            clock,
            journal: Mutex::new(None),
        })
    }

    /// Attach the sync change journal (D20). Mutations start journaling from
    /// this point on; callers run `sync::reconcile` right after to catch up.
    pub fn attach_journal(&self, journal: Arc<Journal>) {
        let mut slot = match self.journal.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(journal);
    }

    /// The attached journal, if sync is enabled.
    pub fn journal(&self) -> Option<Arc<Journal>> {
        match self.journal.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Vault-relative path with `/` separators (journal + sync object paths).
    pub fn rel_path(&self, path: &Path) -> Result<String, CoreError> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| CoreError::Other(format!("{} is outside the vault", path.display())))?;
        Ok(rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"))
    }

    /// Journal hook: no-op unless a journal is attached.
    fn journal_op(
        &self,
        kind: OpKind,
        path: &Path,
        content: Option<&str>,
    ) -> Result<(), CoreError> {
        if let Some(journal) = self.journal() {
            let rel = self.rel_path(path)?;
            let hash = content.map(|c| sync::content_hash(c.as_bytes()));
            journal.append(kind, &rel, hash.as_deref())?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn tusk_dir(&self) -> PathBuf {
        self.root.join(".tusk")
    }

    pub fn now(&self) -> DateTime<Utc> {
        trunc_ms(self.clock.now())
    }

    /// Fresh record in `scope` authored by `author`, ULID from the clock.
    pub fn new_record(&self, kind: RecordType, scope: Scope, author: &str, body: &str) -> Record {
        let now = self.now();
        let id = ulid::Ulid::from_datetime(now.into()).to_string();
        Record::new_at(id, kind, scope, author, now, body.to_string())
    }

    pub fn path_for(&self, record: &Record) -> PathBuf {
        self.memory_dir()
            .join(record.scope.rel_path())
            .join(format!("{}.md", record.id))
    }

    /// Atomic write: temp file in the same directory, then rename, so the
    /// watcher never sees a half-written record (build-loop §3.4). Journals
    /// a `put` (full record state) when sync is enabled.
    pub fn write(&self, record: &Record) -> Result<PathBuf, CoreError> {
        let (path, content) = self.write_file(record)?;
        self.journal_op(OpKind::Put, &path, Some(&content))?;
        Ok(path)
    }

    /// The atomic write itself, sans journaling; returns the written content
    /// so journal hooks hash exactly the bytes on disk.
    fn write_file(&self, record: &Record) -> Result<(PathBuf, String), CoreError> {
        let path = self.path_for(record);
        let dir = path
            .parent()
            .ok_or_else(|| CoreError::Other(format!("no parent for {}", path.display())))?;
        fs::create_dir_all(dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        let content = record.to_markdown()?;
        let tmp = dir.join(format!(".{}.md.tmp", record.id));
        fs::write(&tmp, &content).map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
        fs::rename(&tmp, &path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
        Ok((path, content))
    }

    pub fn load(&self, path: &Path) -> Result<Record, CoreError> {
        let text =
            fs::read_to_string(path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
        Record::from_markdown(&text)
    }

    /// Locate a record file by id (walks the memory tree).
    pub fn find_path(&self, id: &str) -> Result<Option<PathBuf>, CoreError> {
        let needle = format!("{id}.md");
        let mut stack = vec![self.memory_dir()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().and_then(|n| n.to_str()) == Some(needle.as_str()) {
                    return Ok(Some(path));
                }
            }
        }
        Ok(None)
    }

    pub fn get(&self, id: &str) -> Result<(PathBuf, Record), CoreError> {
        let path = self
            .find_path(id)?
            .ok_or_else(|| CoreError::NotFound(id.to_string()))?;
        let rec = self.load(&path)?;
        Ok((path, rec))
    }

    /// All parseable records; files that fail to parse are skipped (partial
    /// writes tolerated per build-loop §3.4).
    pub fn walk(&self) -> Result<Vec<(PathBuf, Record)>, CoreError> {
        let mut out = Vec::new();
        let mut stack = vec![self.memory_dir()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.is_dir() {
                    if !name.starts_with('.') {
                        stack.push(path);
                    }
                } else if name.ends_with(".md") && !name.starts_with('.') {
                    if let Ok(rec) = self.load(&path) {
                        out.push((path, rec));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Set `invalid_at` on a record (bitemporal supersession; body untouched).
    /// Journals a `patch` (metadata mutation) when sync is enabled.
    pub fn invalidate(&self, id: &str, at: DateTime<Utc>) -> Result<Record, CoreError> {
        let (_, mut rec) = self.get(id)?;
        rec.invalid_at = Some(trunc_ms(at));
        let (path, content) = self.write_file(&rec)?;
        self.journal_op(OpKind::Patch, &path, Some(&content))?;
        Ok(rec)
    }

    /// Write `new` with `supersedes = old_id` and invalidate the old record
    /// at the current clock time.
    pub fn supersede(&self, new: &mut Record, old_id: &str) -> Result<PathBuf, CoreError> {
        // Ensure the old record exists before committing anything.
        self.get(old_id)?;
        new.supersedes = Some(old_id.to_string());
        let path = self.write(new)?;
        self.invalidate(old_id, self.clock.now())?;
        Ok(path)
    }

    /// Apply feedback telemetry: `uses += uses_delta`, `successes += success_delta`,
    /// `last_used = at`. Journals a `patch` when sync is enabled.
    pub fn update_telemetry(
        &self,
        id: &str,
        uses_delta: i64,
        success_delta: f64,
        at: DateTime<Utc>,
    ) -> Result<Record, CoreError> {
        let (_, mut rec) = self.get(id)?;
        rec.uses += uses_delta;
        rec.successes += success_delta;
        rec.last_used = Some(trunc_ms(at));
        let (path, content) = self.write_file(&rec)?;
        self.journal_op(OpKind::Patch, &path, Some(&content))?;
        Ok(rec)
    }

    /// Hard delete. Journals a `tombstone` when sync is enabled — forget
    /// never disappears without a trace (D20).
    pub fn forget(&self, id: &str) -> Result<(), CoreError> {
        let (path, _) = self.get(id)?;
        fs::remove_file(&path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
        self.journal_op(OpKind::Tombstone, &path, None)?;
        Ok(())
    }
}

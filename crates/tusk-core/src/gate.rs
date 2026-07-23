//! The gate — intelligence-loop kernel (spec §6): dedup → contradiction
//! probe → policy → commit/queue, plus review ops and graduation.

use crate::error::CoreError;
use crate::index::{Indexer, SearchQuery};
use crate::keyring::pattern_matches;
use crate::record::{body_hash, Record, RecordType};
use crate::scope::Scope;
use crate::vault::VaultStore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Auto,
    Review,
}

/// Per-scope promotion policies. Defaults (spec §6.3): `org` = review,
/// everything else = auto; `type=skill` is always review (handled in
/// `submit`, not here).
#[derive(Debug, Clone, Default)]
pub struct Policies {
    /// (scope pattern, policy) — first match wins; patterns as in grants.
    pub rules: Vec<(String, Policy)>,
}

impl Policies {
    pub fn policy_for(&self, scope: &Scope) -> Policy {
        for (pattern, policy) in &self.rules {
            if pattern_matches(pattern, scope) {
                return *policy;
            }
        }
        match scope {
            Scope::Org => Policy::Review,
            _ => Policy::Auto,
        }
    }
}

/// Graduation thresholds (spec §6, `[graduation]` config).
/// TODO(v1): add a distinct-consumers criterion — v0 intentionally omits it
/// rather than faking it (spec §6).
#[derive(Debug, Clone, Copy)]
pub struct GraduationConfig {
    pub min_uses: i64,
    pub min_success_rate: f64,
}

impl Default for GraduationConfig {
    fn default() -> Self {
        GraduationConfig {
            min_uses: 5,
            min_success_rate: 0.8,
        }
    }
}

fn default_trust() -> f64 {
    0.5
}

/// A candidate submitted to the gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    #[serde(rename = "type")]
    pub kind: RecordType,
    pub scope: Scope,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default = "default_trust")]
    pub trust: f64,
    #[serde(default)]
    pub corrects: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub version: Option<i64>,
}

impl Default for Candidate {
    fn default() -> Self {
        Candidate {
            kind: RecordType::Semantic,
            scope: Scope::User,
            body: String::new(),
            tags: Vec::new(),
            entities: Vec::new(),
            trust: default_trust(),
            corrects: None,
            trigger: None,
            version: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GateOutcome {
    Committed { id: String },
    SupersededExisting { id: String, superseded: String },
    RejectedDuplicate { existing_id: String },
    Queued { qid: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub qid: String,
    pub author: String,
    pub submitted_at: String,
    pub candidate: Candidate,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct QueueFile {
    items: Vec<QueueItem>,
}

/// Token-overlap (Jaccard on lowercase alphanumeric tokens) used by the
/// near-dup/contradiction probe; threshold 0.7 (spec §6.2).
pub fn token_overlap(a: &str, b: &str) -> f64 {
    let toks = |s: &str| -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
            .collect()
    };
    let (ta, tb) = (toks(a), toks(b));
    let union = ta.union(&tb).count();
    if union == 0 {
        return 0.0;
    }
    ta.intersection(&tb).count() as f64 / union as f64
}

pub struct Gate {
    vault: Arc<VaultStore>,
    indexer: Arc<Indexer>,
    policies: Policies,
    queue_path: PathBuf,
    queue_lock: Mutex<()>,
}

impl Gate {
    pub fn new(
        vault: Arc<VaultStore>,
        indexer: Arc<Indexer>,
        policies: Policies,
    ) -> Result<Gate, CoreError> {
        let queue_dir = vault.tusk_dir().join("queue");
        std::fs::create_dir_all(&queue_dir)
            .map_err(|e| CoreError::io(queue_dir.display().to_string(), e))?;
        Ok(Gate {
            vault,
            indexer,
            policies,
            queue_path: queue_dir.join("review.json"),
            queue_lock: Mutex::new(()),
        })
    }

    fn load_queue(&self) -> Result<QueueFile, CoreError> {
        if !self.queue_path.exists() {
            return Ok(QueueFile::default());
        }
        let raw = std::fs::read_to_string(&self.queue_path)
            .map_err(|e| CoreError::io(self.queue_path.display().to_string(), e))?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn save_queue(&self, q: &QueueFile) -> Result<(), CoreError> {
        let tmp = self.queue_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(q)?)
            .map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
        std::fs::rename(&tmp, &self.queue_path)
            .map_err(|e| CoreError::io(self.queue_path.display().to_string(), e))?;
        Ok(())
    }

    /// Gate entry point (spec §6): exact dedup → near-dup probe → policy →
    /// commit or queue.
    pub fn submit(&self, mut candidate: Candidate, author: &str) -> Result<GateOutcome, CoreError> {
        // 1. Exact dedup: one indexed query on body_hash (build-loop §3.5).
        let hash = body_hash(&candidate.body);
        if let Some(existing_id) = self
            .indexer
            .find_valid_by_body_hash(&candidate.scope, &hash)?
            .into_iter()
            .next()
        {
            return Ok(GateOutcome::RejectedDuplicate { existing_id });
        }

        // 2. Near-dup/contradiction probe unless `corrects` was explicit.
        if candidate.corrects.is_none() {
            let probe = self.indexer.search(&SearchQuery {
                query: candidate.body.clone(),
                scopes: Some(vec![candidate.scope.clone()]),
                k: 1,
                ..Default::default()
            })?;
            if let Some(top) = probe.first() {
                if let Ok((_, top_rec)) = self.vault.get(&top.id) {
                    if token_overlap(&candidate.body, &top_rec.body) > 0.7 {
                        candidate.corrects = Some(top_rec.id);
                    }
                }
            }
        }

        // 3. Policy: skills always review (spec §6.3).
        let policy = if candidate.kind == RecordType::Skill {
            Policy::Review
        } else {
            self.policies.policy_for(&candidate.scope)
        };

        match policy {
            Policy::Auto => self.commit(candidate, author),
            Policy::Review => {
                let _guard = self
                    .queue_lock
                    .lock()
                    .map_err(|_| CoreError::Other("queue lock poisoned".into()))?;
                let mut q = self.load_queue()?;
                let qid = format!("q-{}", ulid::Ulid::from_datetime(self.vault.now().into()));
                q.items.push(QueueItem {
                    qid: qid.clone(),
                    author: author.to_string(),
                    submitted_at: self
                        .vault
                        .now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    candidate,
                });
                self.save_queue(&q)?;
                Ok(GateOutcome::Queued { qid })
            }
        }
    }

    /// Step 4, the commit path: write record, invalidate superseded, ingest
    /// to index, materialize skills.
    fn commit(&self, candidate: Candidate, author: &str) -> Result<GateOutcome, CoreError> {
        let mut rec = self.vault.new_record(
            candidate.kind,
            candidate.scope.clone(),
            author,
            &candidate.body,
        );
        rec.tags = candidate.tags;
        rec.entities = candidate.entities;
        rec.trust = candidate.trust;
        rec.trigger = candidate.trigger;
        rec.version = candidate
            .version
            .or(if candidate.kind == RecordType::Skill {
                Some(1)
            } else {
                None
            });

        let outcome = if let Some(old_id) = candidate.corrects {
            let path = self.vault.supersede(&mut rec, &old_id)?;
            self.indexer.ingest_record(&path, &rec)?;
            let (old_path, old_rec) = self.vault.get(&old_id)?;
            self.indexer.ingest_record(&old_path, &old_rec)?;
            GateOutcome::SupersededExisting {
                id: rec.id.clone(),
                superseded: old_id,
            }
        } else {
            let path = self.vault.write(&rec)?;
            self.indexer.ingest_record(&path, &rec)?;
            GateOutcome::Committed { id: rec.id.clone() }
        };

        if rec.kind == RecordType::Skill {
            self.materialize_skill(&rec)?;
        }
        Ok(outcome)
    }

    /// Materialize `skills/<scope-dashed>/<id>/SKILL.md` with `name:` and
    /// `description:` (from trigger) frontmatter (spec §6.4).
    fn materialize_skill(&self, rec: &Record) -> Result<PathBuf, CoreError> {
        let dir = self
            .vault
            .skills_dir()
            .join(rec.scope.dashed())
            .join(&rec.id);
        std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        let name = skill_name(rec);
        let description = rec
            .trigger
            .clone()
            .unwrap_or_else(|| first_line(&rec.body))
            .replace('\n', " ");
        let text = crate::frontmatter::serialize(
            &[
                ("name".to_string(), crate::frontmatter::FmValue::Str(name)),
                (
                    "description".to_string(),
                    crate::frontmatter::FmValue::Str(description),
                ),
            ],
            &rec.body,
        );
        let path = dir.join("SKILL.md");
        std::fs::write(&path, text).map_err(|e| CoreError::io(path.display().to_string(), e))?;
        Ok(path)
    }

    pub fn review_list(&self) -> Result<Vec<QueueItem>, CoreError> {
        Ok(self.load_queue()?.items)
    }

    pub fn queue_depth(&self) -> Result<usize, CoreError> {
        Ok(self.load_queue()?.items.len())
    }

    /// Approve (commit, materializing skills) or reject (drop) a queued item.
    /// Returns `Some(outcome)` on approve, `None` on reject.
    pub fn review(&self, qid: &str, approve: bool) -> Result<Option<GateOutcome>, CoreError> {
        let item = {
            let _guard = self
                .queue_lock
                .lock()
                .map_err(|_| CoreError::Other("queue lock poisoned".into()))?;
            let mut q = self.load_queue()?;
            let idx = q
                .items
                .iter()
                .position(|i| i.qid == qid)
                .ok_or_else(|| CoreError::NotFound(format!("queue item {qid}")))?;
            let item = q.items.remove(idx);
            self.save_queue(&q)?;
            item
        };
        if !approve {
            return Ok(None);
        }
        // Re-check exact dedup at approve time; the vault may have moved on.
        let hash = body_hash(&item.candidate.body);
        if let Some(existing_id) = self
            .indexer
            .find_valid_by_body_hash(&item.candidate.scope, &hash)?
            .into_iter()
            .next()
        {
            return Ok(Some(GateOutcome::RejectedDuplicate { existing_id }));
        }
        self.commit(item.candidate, &item.author).map(Some)
    }

    /// Graduation scanner (spec §6): proven procedural records become skill
    /// candidates, which always land in the review queue.
    pub fn graduate(&self, cfg: &GraduationConfig) -> Result<Vec<GateOutcome>, CoreError> {
        let all = self.vault.walk()?;
        let queue = self.load_queue()?;
        let already: HashSet<String> = queue
            .items
            .iter()
            .flat_map(|i| i.candidate.tags.iter())
            .chain(all.iter().flat_map(|(_, r)| r.tags.iter()))
            .filter(|t| t.starts_with("from:"))
            .cloned()
            .collect();

        let mut out = Vec::new();
        for (_, rec) in &all {
            if rec.kind != RecordType::Procedural || !rec.is_valid_now() {
                continue;
            }
            if rec.uses < cfg.min_uses || rec.success_rate() < cfg.min_success_rate {
                continue;
            }
            let marker = format!("from:{}", rec.id);
            if already.contains(&marker) {
                continue;
            }
            let trigger = first_line(&rec.body);
            let date = self.vault.now().format("%Y-%m-%d");
            let body = format!(
                "{}\n\n---\nGraduated from procedural record {} (uses={}, success_rate={:.2}) on {}.\n",
                rec.body.trim_end(),
                rec.id,
                rec.uses,
                rec.success_rate(),
                date
            );
            let candidate = Candidate {
                kind: RecordType::Skill,
                scope: rec.scope.clone(),
                body,
                tags: vec!["graduated".to_string(), marker],
                entities: rec.entities.clone(),
                trust: rec.trust,
                corrects: None,
                trigger: Some(trigger),
                version: Some(1),
            };
            out.push(self.submit(candidate, &rec.author)?);
        }
        Ok(out)
    }
}

/// First non-empty line, markdown heading markers stripped, ≤120 chars.
fn first_line(body: &str) -> String {
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("untitled")
        .chars()
        .take(120)
        .collect()
}

/// Slug for SKILL.md `name:` — from the first line of the body.
fn skill_name(rec: &Record) -> String {
    let slug: String = first_line(&rec.body)
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let mut cleaned = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash {
                cleaned.push('-');
            }
            prev_dash = true;
        } else {
            cleaned.push(c);
            prev_dash = false;
        }
    }
    let cleaned: String = cleaned.chars().take(60).collect();
    if cleaned.is_empty() {
        format!("skill-{}", rec.id.to_lowercase())
    } else {
        cleaned
    }
}

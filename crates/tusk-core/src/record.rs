use crate::error::CoreError;
use crate::frontmatter::{self, FmValue};
use crate::scope::Scope;
use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordType {
    Episodic,
    Semantic,
    Procedural,
    Skill,
    Profile,
}

impl RecordType {
    pub fn parse(s: &str) -> Result<RecordType, CoreError> {
        Ok(match s {
            "episodic" => RecordType::Episodic,
            "semantic" => RecordType::Semantic,
            "procedural" => RecordType::Procedural,
            "skill" => RecordType::Skill,
            "profile" => RecordType::Profile,
            other => {
                return Err(CoreError::Frontmatter(format!(
                    "unknown record type: {other:?}"
                )))
            }
        })
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RecordType::Episodic => "episodic",
            RecordType::Semantic => "semantic",
            RecordType::Procedural => "procedural",
            RecordType::Skill => "skill",
            RecordType::Profile => "profile",
        };
        f.write_str(s)
    }
}

/// One memory record — one Markdown file in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub id: String,
    pub kind: RecordType,
    pub scope: Scope,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub valid_at: DateTime<Utc>,
    pub invalid_at: Option<DateTime<Utc>>,
    pub supersedes: Option<String>,
    pub entities: Vec<String>,
    pub tags: Vec<String>,
    pub trust: f64,
    pub uses: i64,
    pub successes: f64,
    pub last_used: Option<DateTime<Utc>>,
    pub version: Option<i64>,
    pub trigger: Option<String>,
    pub body: String,
}

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, CoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| CoreError::Frontmatter(format!("bad timestamp {s:?}: {e}")))
}

impl Record {
    /// New record with created_at = valid_at = `at`.
    pub fn new_at(
        id: String,
        kind: RecordType,
        scope: Scope,
        author: &str,
        at: DateTime<Utc>,
        body: String,
    ) -> Record {
        Record {
            id,
            kind,
            scope,
            author: author.to_string(),
            created_at: at,
            valid_at: at,
            invalid_at: None,
            supersedes: None,
            entities: Vec::new(),
            tags: Vec::new(),
            trust: 0.5,
            uses: 0,
            successes: 0.0,
            last_used: None,
            version: None,
            trigger: None,
            body,
        }
    }

    /// sha256 hex of the trimmed body (dedup key; spec §6.1).
    pub fn body_hash(&self) -> String {
        body_hash(&self.body)
    }

    /// Bitemporal validity: `valid_at ≤ t < invalid_at`.
    pub fn is_valid_at(&self, t: DateTime<Utc>) -> bool {
        self.valid_at <= t && self.invalid_at.map(|inv| t < inv).unwrap_or(true)
    }

    pub fn is_valid_now(&self) -> bool {
        self.invalid_at.is_none()
    }

    pub fn success_rate(&self) -> f64 {
        if self.uses > 0 {
            self.successes / self.uses as f64
        } else {
            0.0
        }
    }

    pub fn to_markdown(&self) -> Result<String, CoreError> {
        let mut f: Vec<(String, FmValue)> = Vec::with_capacity(16);
        let mut push = |k: &str, v: FmValue| f.push((k.to_string(), v));
        push("id", FmValue::Str(self.id.clone()));
        push("type", FmValue::Str(self.kind.to_string()));
        push("scope", FmValue::Str(self.scope.to_string()));
        push("author", FmValue::Str(self.author.clone()));
        push("created_at", FmValue::Str(fmt_ts(self.created_at)));
        push("valid_at", FmValue::Str(fmt_ts(self.valid_at)));
        if let Some(t) = self.invalid_at {
            push("invalid_at", FmValue::Str(fmt_ts(t)));
        }
        if let Some(s) = &self.supersedes {
            push("supersedes", FmValue::Str(s.clone()));
        }
        push("entities", FmValue::List(self.entities.clone()));
        push("tags", FmValue::List(self.tags.clone()));
        push("trust", FmValue::Float(self.trust));
        push("uses", FmValue::Int(self.uses));
        push("successes", FmValue::Float(self.successes));
        if let Some(t) = self.last_used {
            push("last_used", FmValue::Str(fmt_ts(t)));
        }
        if let Some(v) = self.version {
            push("version", FmValue::Int(v));
        }
        if let Some(t) = &self.trigger {
            push("trigger", FmValue::Str(t.clone()));
        }
        Ok(frontmatter::serialize(&f, &self.body))
    }

    pub fn from_markdown(text: &str) -> Result<Record, CoreError> {
        let (fields, body) = frontmatter::parse(text)?;
        let get = |k: &str| fields.iter().find(|(key, _)| key == k).map(|(_, v)| v);
        let req_str = |k: &str| -> Result<String, CoreError> {
            match get(k) {
                Some(FmValue::Str(s)) => Ok(s.clone()),
                Some(FmValue::Int(i)) => Ok(i.to_string()),
                Some(FmValue::Float(f)) => Ok(f.to_string()),
                Some(FmValue::List(_)) => Err(CoreError::Frontmatter(format!(
                    "field {k:?} must be a scalar"
                ))),
                None => Err(CoreError::Frontmatter(format!("missing field {k:?}"))),
            }
        };
        let opt_str = |k: &str| -> Result<Option<String>, CoreError> {
            match get(k) {
                None => Ok(None),
                Some(_) => req_str(k).map(Some),
            }
        };
        let list = |k: &str| -> Result<Vec<String>, CoreError> {
            match get(k) {
                Some(FmValue::List(v)) => Ok(v.clone()),
                None => Ok(Vec::new()),
                Some(_) => Err(CoreError::Frontmatter(format!(
                    "field {k:?} must be a list"
                ))),
            }
        };
        let num = |k: &str, default: f64| -> Result<f64, CoreError> {
            match get(k) {
                Some(FmValue::Float(f)) => Ok(*f),
                Some(FmValue::Int(i)) => Ok(*i as f64),
                None => Ok(default),
                Some(_) => Err(CoreError::Frontmatter(format!(
                    "field {k:?} must be numeric"
                ))),
            }
        };
        let int = |k: &str, default: i64| -> Result<i64, CoreError> {
            match get(k) {
                Some(FmValue::Int(i)) => Ok(*i),
                None => Ok(default),
                Some(_) => Err(CoreError::Frontmatter(format!(
                    "field {k:?} must be an integer"
                ))),
            }
        };

        let rec = Record {
            id: req_str("id")?,
            kind: RecordType::parse(&req_str("type")?)?,
            scope: Scope::parse(&req_str("scope")?)?,
            author: req_str("author")?,
            created_at: parse_ts(&req_str("created_at")?)?,
            valid_at: parse_ts(&req_str("valid_at")?)?,
            invalid_at: opt_str("invalid_at")?.map(|s| parse_ts(&s)).transpose()?,
            supersedes: opt_str("supersedes")?,
            entities: list("entities")?,
            tags: list("tags")?,
            trust: num("trust", 0.5)?,
            uses: int("uses", 0)?,
            successes: num("successes", 0.0)?,
            last_used: opt_str("last_used")?.map(|s| parse_ts(&s)).transpose()?,
            version: match get("version") {
                Some(FmValue::Int(i)) => Some(*i),
                None => None,
                Some(_) => {
                    return Err(CoreError::Frontmatter(
                        "field \"version\" must be an integer".into(),
                    ))
                }
            },
            trigger: opt_str("trigger")?,
            body,
        };
        Ok(rec)
    }
}

/// sha256 hex of a trimmed body.
pub fn body_hash(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.trim().as_bytes());
    hex::encode(h.finalize())
}

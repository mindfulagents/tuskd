//! The nine MCP tools (spec §5). Every handler passes through the ACL
//! choke-point `Keyring::can` before touching the store.

use crate::context::TuskContext;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::sync::Arc;
use tusk_core::error::CoreError;
use tusk_core::gate::Candidate;
use tusk_core::index::SearchQuery;
use tusk_core::keyring::Verb;
use tusk_core::record::RecordType;
use tusk_core::scope::Scope;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub is_error: bool,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Tool registry bound to one agent identity.
pub struct ToolRegistry {
    ctx: Arc<TuskContext>,
    agent_id: String,
}

fn denied(reason: impl Into<String>) -> CoreError {
    CoreError::Denied(reason.into())
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, CoreError> {
    arg_str(args, key).ok_or_else(|| CoreError::Other(format!("missing required arg {key:?}")))
}

fn arg_str_list(args: &Value, key: &str) -> Result<Option<Vec<String>>, CoreError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(CoreError::Other(format!(
                            "arg {key:?} must be an array of strings"
                        )))
                    }
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(CoreError::Other(format!(
            "arg {key:?} must be an array of strings"
        ))),
    }
}

fn parse_as_of(args: &Value) -> Result<Option<DateTime<Utc>>, CoreError> {
    match arg_str(args, "as_of") {
        None => Ok(None),
        Some(raw) => DateTime::parse_from_rfc3339(raw)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|e| CoreError::Other(format!("bad as_of {raw:?}: {e}"))),
    }
}

/// Map reflect/promote candidate types to record types: `fact` → semantic,
/// `procedure` → procedural, `correction` → semantic; record-type names pass
/// through.
fn parse_candidate_type(raw: &str) -> Result<RecordType, CoreError> {
    match raw {
        "fact" | "correction" => Ok(RecordType::Semantic),
        "procedure" => Ok(RecordType::Procedural),
        other => RecordType::parse(other),
    }
}

impl ToolRegistry {
    pub fn new(ctx: Arc<TuskContext>, agent_id: String) -> ToolRegistry {
        ToolRegistry { ctx, agent_id }
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn call(&self, name: &str, args: &Value) -> ToolResult {
        let result = self.dispatch(name, args);
        match result {
            Ok(value) => ToolResult {
                is_error: false,
                text: serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}")),
            },
            Err(CoreError::Denied(reason)) => ToolResult {
                is_error: true,
                text: format!("DENIED: {reason}"),
            },
            Err(other) => ToolResult {
                is_error: true,
                text: format!("ERROR: {other}"),
            },
        }
    }

    fn dispatch(&self, name: &str, args: &Value) -> Result<Value, CoreError> {
        // Identity gate first: unknown/revoked agents get nothing.
        let agent = self
            .ctx
            .keyring
            .get(&self.agent_id)?
            .ok_or_else(|| denied(format!("unknown agent {:?}", self.agent_id)))?;
        if agent.revoked {
            return Err(denied(format!("agent {:?} is revoked", self.agent_id)));
        }
        match name {
            "memory_write" => self.memory_write(args),
            "memory_search" => self.memory_search(args),
            "memory_get" => self.memory_get(args),
            "memory_promote" => self.memory_promote(args),
            "memory_reflect" => self.memory_reflect(args),
            "memory_feedback" => self.memory_feedback(args),
            "memory_forget" => self.memory_forget(args),
            "skill_list" => self.skill_list(args),
            "memory_status" => self.memory_status(args),
            other => Err(CoreError::Other(format!("unknown tool {other:?}"))),
        }
    }

    fn can(&self, verb: Verb, scope: &Scope) -> Result<(), CoreError> {
        if self.ctx.keyring.can(&self.agent_id, verb, scope)? {
            Ok(())
        } else {
            Err(denied(format!(
                "agent {:?} lacks {verb} on {scope}",
                self.agent_id
            )))
        }
    }

    /// Own scope + read grants, wildcards expanded against scopes present in
    /// the index (spec §5 memory_search).
    fn entitled_read_scopes(&self) -> Result<Vec<Scope>, CoreError> {
        let mut out = vec![Scope::own(&self.agent_id)];
        let agent = self
            .ctx
            .keyring
            .get(&self.agent_id)?
            .ok_or_else(|| denied("unknown agent"))?;
        let present = self.ctx.indexer.scopes_present()?;
        for pattern in agent.grants.for_verb(Verb::Read) {
            if pattern.ends_with(":*") {
                for scope_str in &present {
                    if let Ok(scope) = Scope::parse(scope_str) {
                        if tusk_core::keyring::pattern_matches(pattern, &scope)
                            && !out.contains(&scope)
                        {
                            out.push(scope);
                        }
                    }
                }
            } else if let Ok(scope) = Scope::parse(pattern) {
                if !out.contains(&scope) {
                    out.push(scope);
                }
            }
        }
        Ok(out)
    }

    fn memory_write(&self, args: &Value) -> Result<Value, CoreError> {
        let scope = match arg_str(args, "scope") {
            Some(raw) => Scope::parse(raw)?,
            None => Scope::own(&self.agent_id),
        };
        self.can(Verb::Write, &scope)?;
        let content = req_str(args, "content")?;
        if content.trim().is_empty() {
            return Err(CoreError::Other("content must not be empty".into()));
        }
        let kind = match arg_str(args, "type") {
            Some(raw) => RecordType::parse(raw)?,
            None => RecordType::Episodic,
        };
        let mut rec = self
            .ctx
            .vault
            .new_record(kind, scope.clone(), &self.agent_id, content);
        if let Some(tags) = arg_str_list(args, "tags")? {
            rec.tags = tags;
        }
        if let Some(entities) = arg_str_list(args, "entities")? {
            rec.entities = entities;
        }
        if let Some(trust) = args.get("trust").and_then(|v| v.as_f64()) {
            rec.trust = trust.clamp(0.0, 1.0);
        }
        if let Some(trigger) = arg_str(args, "trigger") {
            rec.trigger = Some(trigger.to_string());
        }
        if let Some(version) = args.get("version").and_then(|v| v.as_i64()) {
            rec.version = Some(version);
        }

        let path = if let Some(old_id) = arg_str(args, "supersedes") {
            let (_, old) = self.ctx.vault.get(old_id)?;
            self.can(Verb::Write, &old.scope)?;
            let path = self.ctx.vault.supersede(&mut rec, old_id)?;
            let (old_path, old_rec) = self.ctx.vault.get(old_id)?;
            self.ctx.indexer.ingest_record(&old_path, &old_rec)?;
            path
        } else {
            self.ctx.vault.write(&rec)?
        };
        self.ctx.indexer.ingest_record(&path, &rec)?;
        Ok(json!({"id": rec.id, "scope": scope.to_string()}))
    }

    fn memory_search(&self, args: &Value) -> Result<Value, CoreError> {
        let scopes = match arg_str_list(args, "scopes")? {
            Some(raw_scopes) => {
                let mut scopes = Vec::with_capacity(raw_scopes.len());
                for raw in &raw_scopes {
                    let scope = Scope::parse(raw)?;
                    self.can(Verb::Read, &scope)?;
                    scopes.push(scope);
                }
                scopes
            }
            None => self.entitled_read_scopes()?,
        };
        let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10).min(50) as usize;
        let kind = match arg_str(args, "type") {
            Some(raw) => Some(RecordType::parse(raw)?),
            None => None,
        };
        let query = SearchQuery {
            query: arg_str(args, "query").unwrap_or("").to_string(),
            scopes: Some(scopes),
            kind,
            tags: arg_str_list(args, "tags")?,
            as_of: parse_as_of(args)?,
            include_invalid: false,
            k,
        };
        let hits = self.ctx.indexer.search(&query)?;
        Ok(json!({"count": hits.len(), "hits": hits}))
    }

    fn memory_get(&self, args: &Value) -> Result<Value, CoreError> {
        let id = req_str(args, "id")?;
        let (path, rec) = self.ctx.vault.get(id)?;
        self.can(Verb::Read, &rec.scope)?;
        let _ = path;
        Ok(record_json(&rec))
    }

    fn candidate_from_args(&self, args: &Value, scope: Scope) -> Result<Candidate, CoreError> {
        let content = req_str(args, "content")?;
        if content.trim().is_empty() {
            return Err(CoreError::Other("content must not be empty".into()));
        }
        let kind = match arg_str(args, "type") {
            Some(raw) => parse_candidate_type(raw)?,
            None => RecordType::Semantic,
        };
        Ok(Candidate {
            kind,
            scope,
            body: content.to_string(),
            tags: arg_str_list(args, "tags")?.unwrap_or_default(),
            entities: arg_str_list(args, "entities")?.unwrap_or_default(),
            trust: args
                .get("trust")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.5)
                .clamp(0.0, 1.0),
            corrects: arg_str(args, "corrects").map(|s| s.to_string()),
            trigger: arg_str(args, "trigger").map(|s| s.to_string()),
            version: args.get("version").and_then(|v| v.as_i64()),
        })
    }

    /// Promote needs promote OR write on the target scope (spec §5).
    fn check_promote(&self, scope: &Scope) -> Result<(), CoreError> {
        let promote = self.ctx.keyring.can(&self.agent_id, Verb::Promote, scope)?;
        let write = self.ctx.keyring.can(&self.agent_id, Verb::Write, scope)?;
        if promote || write {
            Ok(())
        } else {
            Err(denied(format!(
                "agent {:?} lacks promote/write on {scope}",
                self.agent_id
            )))
        }
    }

    fn memory_promote(&self, args: &Value) -> Result<Value, CoreError> {
        let scope = Scope::parse(req_str(args, "target_scope")?)?;
        self.check_promote(&scope)?;
        let candidate = self.candidate_from_args(args, scope)?;
        let outcome = self.ctx.gate.submit(candidate, &self.agent_id)?;
        Ok(serde_json::to_value(&outcome)?)
    }

    /// The primary loop entry point: each candidate individually gated.
    fn memory_reflect(&self, args: &Value) -> Result<Value, CoreError> {
        let candidates = args
            .get("candidates")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                CoreError::Other("missing required arg \"candidates\" (array)".into())
            })?;
        let default_scope = match arg_str(args, "target_scope") {
            Some(raw) => Some(Scope::parse(raw)?),
            None => None,
        };
        let mut results = Vec::with_capacity(candidates.len());
        for cand_args in candidates {
            let result = (|| -> Result<Value, CoreError> {
                let scope = match arg_str(cand_args, "scope").or(arg_str(cand_args, "target_scope"))
                {
                    Some(raw) => Scope::parse(raw)?,
                    None => default_scope
                        .clone()
                        .unwrap_or_else(|| Scope::own(&self.agent_id)),
                };
                self.check_promote(&scope)?;
                let candidate = self.candidate_from_args(cand_args, scope)?;
                let outcome = self.ctx.gate.submit(candidate, &self.agent_id)?;
                Ok(serde_json::to_value(&outcome)?)
            })();
            results.push(match result {
                Ok(v) => v,
                Err(CoreError::Denied(reason)) => json!({"action": "denied", "reason": reason}),
                Err(other) => json!({"action": "error", "reason": other.to_string()}),
            });
        }
        Ok(json!({"results": results}))
    }

    fn memory_feedback(&self, args: &Value) -> Result<Value, CoreError> {
        let id = req_str(args, "id")?;
        let outcome = req_str(args, "outcome")?;
        let delta = match outcome {
            "success" => 1.0,
            "failure" => 0.0,
            "partial" => 0.5,
            other => {
                return Err(CoreError::Other(format!(
                    "outcome must be success|failure|partial, got {other:?}"
                )))
            }
        };
        let (_, rec) = self.ctx.vault.get(id)?;
        self.can(Verb::Read, &rec.scope)?;
        let now = self.ctx.vault.clock().now();
        let updated = self.ctx.vault.update_telemetry(id, 1, delta, now)?;
        let (path, _) = self.ctx.vault.get(id)?;
        self.ctx.indexer.ingest_record(&path, &updated)?;
        Ok(json!({
            "id": updated.id,
            "uses": updated.uses,
            "successes": updated.successes,
            "success_rate": updated.success_rate(),
            "last_used": updated.last_used.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        }))
    }

    /// Hard delete; only the author or own-scope records (spec §5).
    fn memory_forget(&self, args: &Value) -> Result<Value, CoreError> {
        let id = req_str(args, "id")?;
        let (_, rec) = self.ctx.vault.get(id)?;
        let own = rec.scope == Scope::own(&self.agent_id);
        if !own && rec.author != self.agent_id {
            return Err(denied(format!(
                "memory_forget requires authorship or own scope; {id} is owned by {:?} in {}",
                rec.author, rec.scope
            )));
        }
        self.ctx.vault.forget(id)?;
        self.ctx.indexer.remove(id)?;
        Ok(json!({"id": id, "forgotten": true}))
    }

    fn skill_list(&self, _args: &Value) -> Result<Value, CoreError> {
        let scopes = self.entitled_read_scopes()?;
        let hits = self.ctx.indexer.search(&SearchQuery {
            query: String::new(),
            scopes: Some(scopes),
            kind: Some(RecordType::Skill),
            k: 50,
            ..Default::default()
        })?;
        let skills: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "id": h.id,
                    "scope": h.scope,
                    "trigger": h.trigger,
                    "version": h.version,
                    "uses": h.uses,
                    "successes": h.successes,
                    "success_rate": h.success_rate,
                    "snippet": h.snippet,
                })
            })
            .collect();
        Ok(json!({"count": skills.len(), "skills": skills}))
    }

    fn memory_status(&self, _args: &Value) -> Result<Value, CoreError> {
        let agent = self
            .ctx
            .keyring
            .get(&self.agent_id)?
            .ok_or_else(|| denied("unknown agent"))?;
        let stats = self.ctx.indexer.stats()?;
        let depth = self.ctx.gate.queue_depth()?;
        Ok(json!({
            "agent": {
                "id": agent.id,
                "grants": {
                    "read": agent.grants.read,
                    "write": agent.grants.write,
                    "promote": agent.grants.promote,
                },
            },
            "index": stats,
            "review_queue_depth": depth,
        }))
    }

    pub fn list_tools(&self) -> Vec<ToolDef> {
        tool_defs()
    }
}

fn record_json(rec: &tusk_core::record::Record) -> Value {
    json!({
        "id": rec.id,
        "type": rec.kind.to_string(),
        "scope": rec.scope.to_string(),
        "author": rec.author,
        "created_at": rec.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "valid_at": rec.valid_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "invalid_at": rec.invalid_at.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        "supersedes": rec.supersedes,
        "entities": rec.entities,
        "tags": rec.tags,
        "trust": rec.trust,
        "uses": rec.uses,
        "successes": rec.successes,
        "success_rate": rec.success_rate(),
        "last_used": rec.last_used.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        "version": rec.version,
        "trigger": rec.trigger,
        "body": rec.body,
    })
}

fn obj_schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

fn tool_defs() -> Vec<ToolDef> {
    let scope_prop =
        json!({"type": "string", "description": "agent:<id> | project:<id> | user | org"});
    vec![
        ToolDef {
            name: "memory_write".into(),
            description: "Write a memory record to your own or a write-granted scope. Defaults to your agent scope. Supports supersedes for corrections.".into(),
            input_schema: obj_schema(
                json!({
                    "content": {"type": "string"},
                    "type": {"type": "string", "enum": ["episodic", "semantic", "procedural", "skill", "profile"]},
                    "scope": scope_prop,
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "entities": {"type": "array", "items": {"type": "string"}},
                    "trust": {"type": "number"},
                    "trigger": {"type": "string"},
                    "version": {"type": "integer"},
                    "supersedes": {"type": "string"},
                }),
                &["content"],
            ),
        },
        ToolDef {
            name: "memory_search".into(),
            description: "Hybrid search over entitled scopes with type/tag/as_of filters. k ≤ 50.".into(),
            input_schema: obj_schema(
                json!({
                    "query": {"type": "string"},
                    "scopes": {"type": "array", "items": {"type": "string"}},
                    "type": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "as_of": {"type": "string", "description": "RFC3339 timestamp"},
                    "k": {"type": "integer", "maximum": 50},
                }),
                &["query"],
            ),
        },
        ToolDef {
            name: "memory_get".into(),
            description: "Fetch a record by id (read grant enforced).".into(),
            input_schema: obj_schema(json!({"id": {"type": "string"}}), &["id"]),
        },
        ToolDef {
            name: "memory_promote".into(),
            description: "Submit a candidate through the gate into target_scope (needs promote or write grant). Supports corrects.".into(),
            input_schema: obj_schema(
                json!({
                    "content": {"type": "string"},
                    "type": {"type": "string", "description": "fact | procedure | correction | record type"},
                    "target_scope": scope_prop,
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "entities": {"type": "array", "items": {"type": "string"}},
                    "trust": {"type": "number"},
                    "corrects": {"type": "string"},
                    "trigger": {"type": "string"},
                    "version": {"type": "integer"},
                }),
                &["content", "target_scope"],
            ),
        },
        ToolDef {
            name: "memory_reflect".into(),
            description: "Batch-submit typed candidates (facts/procedures/corrections); each is individually gated. The primary loop entry point.".into(),
            input_schema: obj_schema(
                json!({
                    "candidates": {"type": "array", "items": {"type": "object", "properties": {
                        "type": {"type": "string"},
                        "content": {"type": "string"},
                        "scope": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "entities": {"type": "array", "items": {"type": "string"}},
                        "corrects": {"type": "string"},
                        "trigger": {"type": "string"},
                        "trust": {"type": "number"},
                    }, "required": ["content"]}},
                    "target_scope": {"type": "string", "description": "default scope for candidates that omit one"},
                }),
                &["candidates"],
            ),
        },
        ToolDef {
            name: "memory_feedback".into(),
            description: "Report usage outcome for a record: success | failure | partial. Updates telemetry.".into(),
            input_schema: obj_schema(
                json!({
                    "id": {"type": "string"},
                    "outcome": {"type": "string", "enum": ["success", "failure", "partial"]},
                }),
                &["id", "outcome"],
            ),
        },
        ToolDef {
            name: "memory_forget".into(),
            description: "Hard-delete a record. Only the author or own-scope records.".into(),
            input_schema: obj_schema(json!({"id": {"type": "string"}}), &["id"]),
        },
        ToolDef {
            name: "skill_list".into(),
            description: "List skills across entitled scopes with trigger and telemetry.".into(),
            input_schema: obj_schema(json!({}), &[]),
        },
        ToolDef {
            name: "memory_status".into(),
            description: "Your grants, index stats, and review-queue depth.".into(),
            input_schema: obj_schema(json!({}), &[]),
        },
    ]
}

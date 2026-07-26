//! Admin operations shared by the CLI (embedded) and the daemon's UDS admin
//! endpoint (DECISIONS D6), so there is never a second core owner.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tusk_core::error::CoreError;
use tusk_core::gate::GraduationConfig;
use tusk_core::index::SearchQuery;
use tusk_core::keyring::Verb;
use tusk_core::scope::Scope;
use tusk_mcp::TuskContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum AdminRequest {
    Status,
    Search {
        query: String,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default)]
        as_of: Option<String>,
        #[serde(default)]
        k: Option<usize>,
    },
    Rebuild,
    ReviewList,
    Review {
        qid: String,
        approve: bool,
    },
    Graduate,
    AgentCreate {
        id: String,
        #[serde(default)]
        read: Vec<String>,
        #[serde(default)]
        write: Vec<String>,
        #[serde(default)]
        promote: Vec<String>,
    },
    AgentGrant {
        id: String,
        verb: String,
        scope: String,
    },
    AgentRevoke {
        id: String,
    },
    AgentTokenRotate {
        id: String,
    },
    AgentList,
    RecordList {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        include_invalid: bool,
    },
    RecordGet {
        id: String,
    },
    Forget {
        id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub data: Value,
}

impl AdminResponse {
    pub fn into_result(self) -> Result<Value, CoreError> {
        if self.ok {
            Ok(self.data)
        } else {
            Err(CoreError::Other(
                self.error.unwrap_or_else(|| "unknown admin error".into()),
            ))
        }
    }
}

pub fn execute(
    ctx: &TuskContext,
    graduation: &GraduationConfig,
    req: &AdminRequest,
    daemon: bool,
) -> AdminResponse {
    match execute_inner(ctx, graduation, req, daemon) {
        Ok(data) => AdminResponse {
            ok: true,
            error: None,
            data,
        },
        Err(e) => AdminResponse {
            ok: false,
            error: Some(e.to_string()),
            data: Value::Null,
        },
    }
}

fn execute_inner(
    ctx: &TuskContext,
    graduation: &GraduationConfig,
    req: &AdminRequest,
    daemon: bool,
) -> Result<Value, CoreError> {
    match req {
        AdminRequest::Status => {
            let stats = ctx.indexer.stats()?;
            let agents = ctx.keyring.list()?;
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "vault": ctx.vault.root().display().to_string(),
                "daemon": daemon,
                "index": stats,
                "review_queue_depth": ctx.gate.queue_depth()?,
                "agents": agents.len(),
            }))
        }
        AdminRequest::Search {
            query,
            scopes,
            as_of,
            k,
        } => {
            let mut parsed_scopes = Vec::new();
            for raw in scopes {
                parsed_scopes.push(Scope::parse(raw)?);
            }
            let as_of = match as_of {
                Some(raw) => Some(
                    chrono::DateTime::parse_from_rfc3339(raw)
                        .map(|t| t.with_timezone(&chrono::Utc))
                        .map_err(|e| CoreError::Other(format!("bad as_of {raw:?}: {e}")))?,
                ),
                None => None,
            };
            let hits = ctx.indexer.search(&SearchQuery {
                query: query.clone(),
                scopes: if parsed_scopes.is_empty() {
                    None
                } else {
                    Some(parsed_scopes)
                },
                as_of,
                k: k.unwrap_or(10).min(50),
                ..Default::default()
            })?;
            Ok(json!({"count": hits.len(), "hits": hits}))
        }
        AdminRequest::Rebuild => {
            let count = ctx.indexer.rebuild(&ctx.vault)?;
            Ok(json!({"reindexed": count}))
        }
        AdminRequest::ReviewList => {
            let items = ctx.gate.review_list()?;
            Ok(serde_json::to_value(items)?)
        }
        AdminRequest::Review { qid, approve } => {
            let outcome = ctx.gate.review(qid, *approve)?;
            match outcome {
                Some(o) => Ok(serde_json::to_value(o)?),
                None => Ok(json!({"action": "rejected", "qid": qid})),
            }
        }
        AdminRequest::Graduate => {
            let outcomes = ctx.gate.graduate(graduation)?;
            Ok(serde_json::to_value(outcomes)?)
        }
        AdminRequest::AgentCreate {
            id,
            read,
            write,
            promote,
        } => {
            fn as_refs(v: &[String]) -> Vec<&str> {
                v.iter().map(|s| s.as_str()).collect()
            }
            let created =
                ctx.keyring
                    .create(id, &as_refs(read), &as_refs(write), &as_refs(promote))?;
            Ok(json!({
                "id": created.id,
                "token": created.token,
                "private_key_pem": created.private_key_pem,
                "public_key_pem": created.public_key_pem,
            }))
        }
        AdminRequest::AgentGrant { id, verb, scope } => {
            ctx.keyring.grant(id, Verb::parse(verb)?, scope)?;
            Ok(json!({"id": id, "granted": format!("{verb} {scope}")}))
        }
        AdminRequest::AgentRevoke { id } => {
            ctx.keyring.revoke(id)?;
            Ok(json!({"id": id, "revoked": true}))
        }
        AdminRequest::AgentTokenRotate { id } => {
            let token = ctx.keyring.rotate_token(id)?;
            Ok(json!({"id": id, "token": token}))
        }
        AdminRequest::RecordList {
            scope,
            kind,
            limit,
            offset,
            include_invalid,
        } => {
            let scopes = match scope {
                Some(raw) => Some(vec![Scope::parse(raw)?]),
                None => None,
            };
            let kind = match kind.as_deref() {
                Some(raw) => Some(tusk_core::record::RecordType::parse(raw)?),
                None => None,
            };
            // The underlying browse query is capped at 500 rows; page inside it.
            let limit = limit.unwrap_or(50).clamp(1, 200);
            let offset = offset.unwrap_or(0).min(500 - 1);
            let hits = ctx.indexer.search(&SearchQuery {
                query: String::new(),
                scopes,
                kind,
                include_invalid: *include_invalid,
                k: (offset + limit).min(500),
                ..Default::default()
            })?;
            let records: Vec<_> = hits.into_iter().skip(offset).collect();
            Ok(json!({"count": records.len(), "offset": offset, "records": records}))
        }
        AdminRequest::RecordGet { id } => {
            let (path, rec) = ctx.vault.get(id)?;
            let mut out = tusk_mcp::record_json(&rec);
            out["path"] = json!(path.display().to_string());
            Ok(out)
        }
        AdminRequest::Forget { id } => {
            ctx.vault.forget(id)?;
            ctx.indexer.remove(id)?;
            Ok(json!({"id": id, "forgotten": true}))
        }
        AdminRequest::AgentList => {
            let agents = ctx.keyring.list()?;
            let list: Vec<Value> = agents
                .iter()
                .map(|a| {
                    json!({
                        "id": a.id,
                        "grants": {
                            "read": a.grants.read,
                            "write": a.grants.write,
                            "promote": a.grants.promote,
                        },
                        "created_at": a.created_at,
                        "revoked": a.revoked,
                    })
                })
                .collect();
            Ok(json!(list))
        }
    }
}

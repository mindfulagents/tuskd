//! Web dashboard (DECISIONS D14): embedded `/ui` shell + operator-token
//! `/api` plane. Every mutation goes through `admin::execute` — the same
//! plane the CLI and UDS use — so the dashboard cannot bypass core rules.

use crate::admin::{self, AdminRequest};
use crate::config::Config;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tusk_core::error::CoreError;
use tusk_core::keyring::ct_eq_digest;
use tusk_mcp::TuskContext;

const INDEX_HTML: &str = include_str!("ui/index.html");

#[derive(Clone)]
pub struct DashState {
    pub ctx: Arc<TuskContext>,
    pub config: Arc<Config>,
    pub token_sha256: Arc<[u8; 32]>,
    pub started: Instant,
}

/// Operator token minted at daemon start; only its sha256 is kept in memory.
pub struct OperatorToken {
    pub token: String,
    pub sha256: [u8; 32],
}

pub fn generate_token() -> OperatorToken {
    let mut secret = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    let token = format!("tuskop_{}", hex::encode(secret));
    let sha256 = Sha256::digest(token.as_bytes()).into();
    OperatorToken { token, sha256 }
}

pub fn token_file_path(vault: &Path) -> PathBuf {
    vault.join(".tusk").join("admin-token")
}

/// Write `.tusk/admin-token` (owner-only) and return the dashboard URL.
pub fn write_token_file(vault: &Path, http_addr: &str, token: &str) -> Result<String, CoreError> {
    let url = format!("http://{http_addr}/ui/#t={token}");
    let contents = serde_json::to_string_pretty(&json!({
        "url": url,
        "http": http_addr,
        "token": token,
    }))?;
    crate::platform::write_private(&token_file_path(vault), &contents)?;
    Ok(url)
}

pub fn remove_token_file(vault: &Path) {
    let _ = std::fs::remove_file(token_file_path(vault));
}

pub fn routes(state: DashState) -> Router {
    Router::new()
        .route("/ui", get(ui_index))
        .route("/ui/", get(ui_index))
        .route("/api/admin", post(api_admin))
        .route("/api/meta", get(api_meta))
        .route("/api/config", get(api_config))
        .route("/api/export", get(api_export))
        .with_state(state)
}

async fn ui_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Constant-time operator-token check on the Authorization header.
fn authorized(state: &DashState, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| ct_eq_digest(&Sha256::digest(t.as_bytes()), state.token_sha256.as_slice()))
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized"})),
    )
        .into_response()
}

/// The whole admin plane over one endpoint: body is an `AdminRequest`.
async fn api_admin(
    State(state): State<DashState>,
    headers: HeaderMap,
    Json(req): Json<AdminRequest>,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    let resp = admin::execute(&state.ctx, &state.config.graduation, &req, true);
    Json(resp).into_response()
}

async fn api_meta(State(state): State<DashState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "vault": state.ctx.vault.root().display().to_string(),
        "uptime_secs": state.started.elapsed().as_secs(),
        "graduation_interval_hours": state.config.graduation_interval_hours,
    }))
    .into_response()
}

async fn api_config(State(state): State<DashState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    let cfg = &state.config;
    let policies: Vec<Value> = cfg
        .policies
        .rules
        .iter()
        .map(|(pattern, policy)| json!({"pattern": pattern, "policy": format!("{policy:?}").to_lowercase()}))
        .collect();
    Json(json!({
        "vault": cfg.vault.display().to_string(),
        "http_port": cfg.http_port,
        "uds_path": cfg.uds_path.display().to_string(),
        "policies": policies,
        "graduation": {
            "min_uses": cfg.graduation.min_uses,
            "min_success_rate": cfg.graduation.min_success_rate,
            "interval_hours": cfg.graduation_interval_hours,
        },
        "ranking": {
            "uses_weight": cfg.ranking.uses_weight,
            "trust_weight": cfg.ranking.trust_weight,
        },
    }))
    .into_response()
}

/// Vault export as a tar.gz download (same archive the CLI writes).
async fn api_export(State(state): State<DashState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    let vault = state.ctx.vault.root().to_path_buf();
    let tmp = std::env::temp_dir().join(format!("tuskd-export-{}.tar.gz", std::process::id()));
    let tmp_for_task = tmp.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CoreError> {
        crate::archive::export(&vault, &tmp_for_task)?;
        let bytes = std::fs::read(&tmp_for_task)
            .map_err(|e| CoreError::io(tmp_for_task.display().to_string(), e))?;
        Ok(bytes)
    })
    .await;
    let _ = std::fs::remove_file(&tmp);
    match result {
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/gzip".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"tuskd-vault-export.tar.gz\"".to_string(),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("export task: {e}")})),
        )
            .into_response(),
    }
}

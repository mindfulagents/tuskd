//! `tuskd start` — the single-owner daemon (spec §2.1): axum `/status` +
//! `/mcp` (streamable HTTP, stateless per POST — D8), UDS server for local
//! sessions and admin one-shots, graduation timer, clean shutdown.

use crate::admin::{self, AdminRequest};
use crate::config::Config;
use crate::mcp_protocol::{is_notification, McpSession};
use crate::runtime::CoreHost;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tusk_core::error::CoreError;
use tusk_mcp::{ToolRegistry, TuskContext};

#[derive(Clone)]
struct AppState {
    ctx: Arc<TuskContext>,
    config: Arc<Config>,
}

pub fn run(config: Config) -> Result<(), CoreError> {
    let host = CoreHost::open(&config, true)?;
    let ctx = Arc::clone(&host.ctx);
    let config = Arc::new(config);

    // D28 auto-sync worker handle; spawned inside the async block only
    // after both listeners bind (D32 — a daemon that fails to boot must
    // not have started syncing).
    let mut sync_worker: Option<crate::sync_worker::SyncWorker> = None;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CoreError::Other(format!("tokio runtime: {e}")))?;

    let result = runtime.block_on(async {
        // UDS listener (remove any stale socket first).
        let _ = std::fs::remove_file(&config.uds_path);
        let uds = UnixListener::bind(&config.uds_path)
            .map_err(|e| CoreError::io(config.uds_path.display().to_string(), e))?;

        // HTTP listener; port 0 = ephemeral. The stock default port falls
        // back to an ephemeral one when busy (D32) — a second vault on
        // one machine must not die on the first vault's port.
        let (tcp, fell_back) =
            bind_http(config.http_port, crate::config::DEFAULT_HTTP_PORT).await?;
        let actual = tcp
            .local_addr()
            .map_err(|e| CoreError::Other(format!("local_addr: {e}")))?;
        if fell_back {
            eprintln!(
                "port {} is taken (another vault's daemon?) — using {actual} instead; \
                 set http_port in .tusk/tuskd.toml to pin one",
                config.http_port
            );
        }

        // Operator token for the dashboard (D14): minted per daemon run,
        // written owner-only to .tusk/admin-token before the banner.
        let operator = crate::dashboard::generate_token();
        let dashboard_url = crate::dashboard::write_token_file(
            &config.vault,
            &actual.to_string(),
            &operator.token,
        )?;

        // Banner: first stdout line, machine-readable.
        println!(
            "{}",
            json!({
                "event": "listening",
                "http": actual.to_string(),
                "uds": config.uds_path.display().to_string(),
                "vault": config.vault.display().to_string(),
                "dashboard": dashboard_url,
            })
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
        eprintln!("dashboard: {dashboard_url}");

        let state = AppState {
            ctx: Arc::clone(&ctx),
            config: Arc::clone(&config),
        };
        let app = Router::new()
            .route("/status", get(http_status))
            .route("/mcp", post(http_mcp))
            .with_state(state)
            .merge(crate::dashboard::routes(crate::dashboard::DashState {
                ctx: Arc::clone(&ctx),
                config: Arc::clone(&config),
                token_sha256: Arc::new(operator.sha256),
                started: std::time::Instant::now(),
            }));

        // Graduation timer (default 24h).
        {
            let ctx = Arc::clone(&ctx);
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                let period =
                    std::time::Duration::from_secs(config.graduation_interval_hours * 3600);
                let mut interval = tokio::time::interval(period);
                interval.tick().await; // consume the immediate first tick
                loop {
                    interval.tick().await;
                    match ctx.gate.graduate(&config.graduation) {
                        Ok(outcomes) if !outcomes.is_empty() => {
                            eprintln!("graduation: {} candidate(s) queued", outcomes.len());
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("graduation scan failed: {e}"),
                    }
                }
            });
        }

        // D28: auto-sync worker — a plain thread (the sync clients are
        // blocking HTTP), started only when this vault is connected to a
        // cloud repo and `[sync] auto` is not disabled. Spawned only now,
        // with both listeners bound.
        if config.sync_auto && crate::sync_worker::is_connected(&config.vault) {
            eprintln!(
                "sync: auto-sync every {}s (disable with [sync] auto = false)",
                config.sync_interval_secs
            );
            sync_worker = Some(crate::sync_worker::SyncWorker::spawn(
                config.vault.clone(),
                std::time::Duration::from_secs(config.sync_interval_secs),
            ));
        }

        // Shutdown request channel (D18): `tuskd stop` over UDS.
        let stop = Arc::new(tokio::sync::Notify::new());

        // UDS accept loop.
        {
            let ctx = Arc::clone(&ctx);
            let config = Arc::clone(&config);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                while let Ok((stream, _)) = uds.accept().await {
                    let ctx = Arc::clone(&ctx);
                    let config = Arc::clone(&config);
                    let stop = Arc::clone(&stop);
                    tokio::spawn(async move {
                        let _ = handle_uds(stream, ctx, config, stop).await;
                    });
                }
            });
        }

        axum::serve(tcp, app)
            .with_graceful_shutdown(shutdown_signal(stop))
            .await
            .map_err(|e| CoreError::Other(format!("http serve: {e}")))
    });

    let _ = std::fs::remove_file(&config.uds_path);
    crate::dashboard::remove_token_file(&config.vault);
    if let Some(worker) = sync_worker {
        worker.stop();
    }
    host.shutdown();
    eprintln!("tuskd: shut down cleanly");
    result
}

/// Bind the HTTP listener. When the *stock* default port is busy, fall
/// back to an ephemeral port (returning `fell_back = true`); a custom
/// port fails hard — the user asked for that port and deserves the truth.
pub async fn bind_http(
    port: u16,
    default_port: u16,
) -> Result<(tokio::net::TcpListener, bool), CoreError> {
    let addr = format!("127.0.0.1:{port}");
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(tcp) => Ok((tcp, false)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port == default_port => {
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| CoreError::Other(format!("bind 127.0.0.1:0: {e}")))?;
            Ok((tcp, true))
        }
        Err(e) => Err(CoreError::Other(format!("bind {addr}: {e}"))),
    }
}

async fn shutdown_signal(stop: Arc<tokio::sync::Notify>) {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(s) => s,
        Err(_) => return stop.notified().await,
    };
    tokio::select! {
        _ = sigterm.recv() => {},
        _ = tokio::signal::ctrl_c() => {},
        _ = stop.notified() => {},
    }
}

async fn http_status(State(state): State<AppState>) -> impl IntoResponse {
    let resp = admin::execute(
        &state.ctx,
        &state.config.graduation,
        &AdminRequest::Status,
        true,
    );
    if resp.ok {
        (StatusCode::OK, Json(resp.data))
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": resp.error})),
        )
    }
}

/// Bearer → agent → one JSON-RPC message per POST (D8).
async fn http_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let agent = match token.and_then(|t| state.ctx.keyring.auth_by_token(t).ok()) {
        Some(agent) => agent,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            );
        }
    };
    let msg: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"jsonrpc": "2.0", "id": null,
                            "error": {"code": -32700, "message": format!("parse error: {e}")}})),
            );
        }
    };
    if is_notification(&msg) {
        return (StatusCode::ACCEPTED, Json(Value::Null));
    }
    let session = McpSession::new(ToolRegistry::new(Arc::clone(&state.ctx), agent.id));
    match session.handle_value(&msg) {
        Some(resp) => (StatusCode::OK, Json(resp)),
        None => (StatusCode::ACCEPTED, Json(Value::Null)),
    }
}

/// UDS connections: first line is either an admin one-shot or a session
/// header; sessions then speak newline-delimited JSON-RPC.
async fn handle_uds(
    stream: UnixStream,
    ctx: Arc<TuskContext>,
    config: Arc<Config>,
    stop: Arc<tokio::sync::Notify>,
) -> Result<(), CoreError> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let first = match lines
        .next_line()
        .await
        .map_err(|e| CoreError::Other(format!("uds read: {e}")))?
    {
        Some(l) => l,
        None => return Ok(()),
    };
    let header: Value = match serde_json::from_str(&first) {
        Ok(v) => v,
        Err(_) => {
            let _ = write_half
                .write_all(b"{\"ok\":false,\"error\":\"bad header\"}\n")
                .await;
            return Ok(());
        }
    };

    if let Some(admin_req) = header.get("tusk_admin") {
        let parsed = serde_json::from_value::<AdminRequest>(admin_req.clone());
        // D18: shutdown is a daemon-lifecycle concern, not a core operation —
        // ack first, then trigger the graceful-shutdown path.
        if matches!(parsed, Ok(AdminRequest::Shutdown)) {
            let _ = write_half
                .write_all(b"{\"ok\":true,\"data\":{\"stopping\":true}}\n")
                .await;
            stop.notify_one();
            return Ok(());
        }
        let resp = match parsed {
            Ok(req) => admin::execute(&ctx, &config.graduation, &req, true),
            Err(e) => crate::admin::AdminResponse {
                ok: false,
                error: Some(format!("bad admin request: {e}")),
                data: Value::Null,
            },
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        let _ = write_half.write_all(out.as_bytes()).await;
        return Ok(());
    }

    if let Some(session_hdr) = header.get("tusk_session") {
        let agent_id = session_hdr
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let valid = matches!(ctx.keyring.get(agent_id), Ok(Some(a)) if !a.revoked);
        if !valid {
            let _ = write_half
                .write_all(
                    format!(
                        "{}\n",
                        json!({"ok": false, "error": format!("unknown or revoked agent {agent_id:?}")})
                    )
                    .as_bytes(),
                )
                .await;
            return Ok(());
        }
        let _ = write_half
            .write_all(format!("{}\n", json!({"ok": true})).as_bytes())
            .await;
        let session = McpSession::new(ToolRegistry::new(Arc::clone(&ctx), agent_id.to_string()));
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(resp) = session.handle_line(&line) {
                if write_half
                    .write_all(format!("{resp}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
        return Ok(());
    }

    let _ = write_half
        .write_all(b"{\"ok\":false,\"error\":\"unknown header\"}\n")
        .await;
    Ok(())
}

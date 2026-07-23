//! `tuskd mcp --agent <id>` — stdio⇄UDS proxy when a daemon owns the
//! vault, embedded core otherwise (spec §2.1). Must exit promptly when stdin
//! closes (build-loop §3.1).

use crate::config::Config;
use crate::mcp_protocol::McpSession;
use crate::runtime::CoreHost;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use tusk_core::error::CoreError;
use tusk_mcp::ToolRegistry;

pub fn run(config: Config, agent: String) -> Result<(), CoreError> {
    if let Ok(stream) = UnixStream::connect(&config.uds_path) {
        return proxy(stream, &agent);
    }
    embedded(config, agent)
}

/// Thin proxy: header/ack handshake, then pump stdin→UDS and UDS→stdout.
fn proxy(stream: UnixStream, agent: &str) -> Result<(), CoreError> {
    let mut writer = stream
        .try_clone()
        .map_err(|e| CoreError::Other(format!("uds clone: {e}")))?;
    writeln!(writer, "{}", json!({"tusk_session": {"agent": agent}}))
        .map_err(|e| CoreError::Other(format!("uds write: {e}")))?;

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| CoreError::Other(format!("uds clone: {e}")))?,
    );
    let mut ack = String::new();
    reader
        .read_line(&mut ack)
        .map_err(|e| CoreError::Other(format!("uds ack: {e}")))?;
    let ack_val: Value =
        serde_json::from_str(ack.trim()).map_err(|e| CoreError::Other(format!("bad ack: {e}")))?;
    if ack_val.get("ok") != Some(&Value::Bool(true)) {
        let reason = ack_val
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("session refused");
        return Err(CoreError::Denied(reason.to_string()));
    }

    // UDS → stdout.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let mut out = stdout.lock();
                    if out.write_all(line.as_bytes()).is_err() || out.flush().is_err() {
                        break;
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });

    // stdin → UDS; on EOF, close the write side so the daemon ends the session.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if writeln!(writer, "{line}").is_err() {
            break;
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    // Give in-flight responses a moment, then exit regardless.
    let _ = done_rx.recv_timeout(std::time::Duration::from_secs(1));
    Ok(())
}

/// Embedded single-user mode: own the core (advisory lock), serve one stdio
/// session, tear everything down when stdin closes.
fn embedded(config: Config, agent: String) -> Result<(), CoreError> {
    let host = CoreHost::open(&config, true)?;
    let valid = matches!(host.ctx.keyring.get(&agent), Ok(Some(a)) if !a.revoked);
    if !valid {
        host.shutdown();
        return Err(CoreError::Denied(format!(
            "unknown or revoked agent {agent:?}"
        )));
    }
    let session = McpSession::new(ToolRegistry::new(Arc::clone(&host.ctx), agent));

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = session.handle_line(&line) {
            let mut out = stdout.lock();
            if writeln!(out, "{resp}").is_err() || out.flush().is_err() {
                break;
            }
        }
    }
    // stdin closed: stop watcher, release lock, exit (build-loop §3.1).
    host.shutdown();
    Ok(())
}

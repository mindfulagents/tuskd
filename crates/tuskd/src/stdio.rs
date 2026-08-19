//! `tuskd mcp --agent <id>` — stdio⇄UDS proxy when a daemon owns the
//! vault, embedded core otherwise (spec §2.1). Must exit promptly when stdin
//! closes (build-loop §3.1).
//!
//! D40: the two modes are not a one-time choice. An embedded session yields
//! the vault when a daemon (or a one-shot command) asks for it, and
//! re-attaches to the daemon as a proxy; a proxy whose daemon goes away
//! falls back to embedded. The client on the other end of stdio never
//! notices — it keeps one MCP session for as long as it keeps stdin open.

use crate::config::Config;
use crate::mcp_protocol::McpSession;
use crate::platform::{handoff_request_path, VaultLock};
use crate::runtime::CoreHost;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tusk_core::error::CoreError;
use tusk_mcp::ToolRegistry;

/// How often an idle embedded session looks for a handoff request.
const YIELD_POLL: Duration = Duration::from_millis(250);
/// After yielding, how long to wait for the requester to actually take the
/// vault before concluding the request was stale and taking it back.
const TAKEOVER_WAIT: Duration = Duration::from_secs(5);
/// After yielding to a one-shot borrower, how long to wait for it to finish.
const BORROW_WAIT: Duration = Duration::from_secs(60);
/// When the daemon drops a proxied session, how long to wait for its lock
/// to be released before giving up on going embedded.
const DAEMON_EXIT_WAIT: Duration = Duration::from_secs(10);

/// Why a serving loop returned.
enum Next {
    /// stdin closed: the session is over.
    Done,
    /// Embedded: someone asked for the vault and we released it.
    Yielded,
    /// Proxy: the daemon closed the connection. Carries the requests it
    /// never answered, to be replayed by whoever serves next.
    DaemonGone(Vec<String>),
}

pub fn run(config: Config, agent: String) -> Result<(), CoreError> {
    let lines = spawn_stdin_reader();
    let mut replay: Vec<String> = Vec::new();
    loop {
        if let Ok(stream) = UnixStream::connect(&config.uds_path) {
            match proxy(stream, &agent, &lines, std::mem::take(&mut replay))? {
                Next::Done => return Ok(()),
                Next::DaemonGone(unanswered) => {
                    replay = unanswered;
                    wait_for_lock_release(&config.vault, DAEMON_EXIT_WAIT);
                    continue;
                }
                Next::Yielded => unreachable!("proxy never yields"),
            }
        }
        match embedded(&config, &agent, &lines, std::mem::take(&mut replay))? {
            Next::Done => return Ok(()),
            Next::Yielded => wait_for_takeover(&config)?,
            Next::DaemonGone(_) => unreachable!("embedded has no daemon"),
        }
    }
}

/// JSON-RPC id of a request line, if it is one (notifications have none).
fn request_id(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    v.get("method")?;
    v.get("id").cloned()
}

/// Requests written to the daemon that have not been answered yet. When
/// the daemon goes away mid-flight, these are replayed to the next server
/// instead of silently vanishing on the client.
#[derive(Default)]
struct InFlight(Vec<(Value, String)>);

impl InFlight {
    fn sent(&mut self, line: &str) {
        if let Some(id) = request_id(line) {
            self.0.push((id, line.to_string()));
        }
    }
    fn answered(&mut self, response_line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(response_line) else {
            return;
        };
        if let Some(id) = v.get("id") {
            self.0.retain(|(pending, _)| pending != id);
        }
    }
    fn drain(&mut self) -> Vec<String> {
        self.0.drain(..).map(|(_, line)| line).collect()
    }
}

/// Feed stdin lines through a channel so the serving loops can wait on
/// "a line or a timer" instead of blocking in `read_line`. The channel
/// closes when stdin does.
fn spawn_stdin_reader() -> Receiver<String> {
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Thin proxy: header/ack handshake, then pump stdin→UDS and UDS→stdout.
fn proxy(
    stream: UnixStream,
    agent: &str,
    lines: &Receiver<String>,
    replay: Vec<String>,
) -> Result<Next, CoreError> {
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

    let in_flight = Arc::new(Mutex::new(InFlight::default()));

    // UDS → stdout.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    {
        let in_flight = Arc::clone(&in_flight);
        std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(mut f) = in_flight.lock() {
                            f.answered(line.trim());
                        }
                        let mut out = stdout.lock();
                        if out.write_all(line.as_bytes()).is_err() || out.flush().is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = done_tx.send(());
        });
    }

    let daemon_gone = |in_flight: &Arc<Mutex<InFlight>>| {
        let unanswered = in_flight.lock().map(|mut f| f.drain()).unwrap_or_default();
        Ok(Next::DaemonGone(unanswered))
    };
    let mut send = |line: &str| -> bool {
        if let Ok(mut f) = in_flight.lock() {
            f.sent(line);
        }
        writeln!(writer, "{line}").is_ok()
    };

    // Requests a previous server never answered go first.
    for line in &replay {
        if !send(line) {
            return daemon_gone(&in_flight);
        }
    }

    // stdin → UDS. Two ways out: stdin closes (end the session), or the
    // daemon hangs up on us (go embedded so the client keeps working).
    loop {
        match lines.recv_timeout(YIELD_POLL) {
            Ok(line) => {
                if !send(&line) {
                    return daemon_gone(&in_flight);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if done_rx.try_recv().is_ok() {
                    return daemon_gone(&in_flight);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // On EOF, close the write side so the daemon ends the session, give
    // in-flight responses a moment, then exit regardless.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = done_rx.recv_timeout(Duration::from_secs(1));
    Ok(Next::Done)
}

/// Embedded single-user mode: own the core (advisory lock), serve one stdio
/// session, tear everything down when stdin closes — or when asked to
/// yield the vault (D40).
fn embedded(
    config: &Config,
    agent: &str,
    lines: &Receiver<String>,
    replay: Vec<String>,
) -> Result<Next, CoreError> {
    let host = CoreHost::open(config, true)?;
    let valid = matches!(host.ctx.keyring.get(agent), Ok(Some(a)) if !a.revoked);
    if !valid {
        host.shutdown();
        return Err(CoreError::Denied(format!(
            "unknown or revoked agent {agent:?}"
        )));
    }
    let session = McpSession::new(ToolRegistry::new(Arc::clone(&host.ctx), agent.to_string()));
    let request = handoff_request_path(&config.vault);

    let stdout = std::io::stdout();
    let serve = |line: &str| -> bool {
        match session.handle_line(line) {
            Some(resp) => {
                let mut out = stdout.lock();
                writeln!(out, "{resp}").is_ok() && out.flush().is_ok()
            }
            None => true,
        }
    };
    // Requests a previous server never answered go first.
    let mut replay = replay.into_iter();
    let next = loop {
        if let Some(line) = replay.next() {
            if !serve(&line) {
                break Next::Done;
            }
            continue;
        }
        // Between messages — never mid-request — check whether someone
        // wants the vault.
        if request.exists() {
            break Next::Yielded;
        }
        let line = match lines.recv_timeout(YIELD_POLL) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break Next::Done,
        };
        if line.trim().is_empty() {
            continue;
        }
        if !serve(&line) {
            break Next::Done;
        }
    };
    // Stop watcher, release lock (build-loop §3.1).
    host.shutdown();
    Ok(next)
}

/// We released the vault because someone asked. Watch what happens next:
/// a daemon starts listening → the caller proxies to it; the request is
/// gone and the lock is free again → a one-shot borrower finished, the
/// caller re-embeds; nobody takes it → the request was stale, drop it and
/// re-embed.
fn wait_for_takeover(config: &Config) -> Result<(), CoreError> {
    let request = handoff_request_path(&config.vault);
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if UnixStream::connect(&config.uds_path).is_ok() {
            return Ok(());
        }
        let requested = request.exists();
        if !requested {
            // Requester took the lock (and cleared its request) — wait for
            // it to finish, unless it turns out to be a daemon booting.
            if lock_is_free(&config.vault) {
                return Ok(());
            }
            if start.elapsed() > BORROW_WAIT {
                return Err(CoreError::Other(
                    "yielded the vault for a handoff but it was never released".into(),
                ));
            }
        } else if start.elapsed() > TAKEOVER_WAIT {
            // Nobody came for it: stale request (a requester that died).
            let _ = std::fs::remove_file(&request);
            return Ok(());
        }
    }
}

fn lock_is_free(vault: &std::path::Path) -> bool {
    VaultLock::acquire(vault).is_ok()
}

/// The daemon we were proxying through went away; its lock lingers for a
/// moment while it drains. Give it time before we try to go embedded.
fn wait_for_lock_release(vault: &std::path::Path, wait: Duration) {
    let deadline = Instant::now() + wait;
    while !lock_is_free(vault) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

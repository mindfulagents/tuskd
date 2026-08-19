//! D40 — embedded MCP sessions yield the vault lock on request.
//!
//! Drives the real `tuskd` binary. The scenario that motivated this: a
//! desktop client (Claude Desktop) spawned `tuskd mcp` while no daemon
//! was running, the session went embedded and took `.tusk/lock`, and from
//! then on `tuskd start` — and even `tuskd status` — failed until the
//! client was quit.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tuskd"))
}

fn run(vault: &std::path::Path, args: &[&str]) -> std::process::Output {
    bin()
        .arg("--vault")
        .arg(vault)
        .args(args)
        .output()
        .expect("spawn tuskd")
}

fn run_ok(vault: &std::path::Path, args: &[&str]) -> String {
    let out = run(vault, args);
    assert!(
        out.status.success(),
        "tuskd {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn init_vault(vault: &std::path::Path) {
    run_ok(vault, &["init"]);
    run_ok(vault, &["agent", "create", "solo"]);
    // Ephemeral dashboard port so parallel tests never collide.
    let cfg_path = vault.join(".tusk").join("tuskd.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    std::fs::write(&cfg_path, cfg.replace("http_port = 7477", "http_port = 0")).unwrap();
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Session {
    /// Spawn `tuskd mcp --agent solo` and complete the MCP handshake.
    fn open(vault: &std::path::Path) -> Session {
        let mut child = bin()
            .arg("--vault")
            .arg(vault)
            .args(["mcp", "--agent", "solo"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Session {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 1,
        };
        let resp = s.request(json!({"method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "d40-test", "version": "0"}}}));
        assert_eq!(resp["result"]["serverInfo"]["name"], "tuskd");
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{msg}").unwrap();
    }

    fn request(&mut self, mut msg: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        msg["jsonrpc"] = json!("2.0");
        msg["id"] = json!(id);
        self.send(&msg);
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("bad response ({e}): {line}"));
        assert_eq!(resp["id"], json!(id), "response out of order: {resp}");
        resp
    }

    /// `memory_status` round trip — proves the session is being served.
    fn status(&mut self) -> Value {
        let resp = self.request(json!({"method": "tools/call",
            "params": {"name": "memory_status", "arguments": {}}}));
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait_exit(&mut self, secs: u64) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "process did not exit within {secs}s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill").arg(self.0.id().to_string()).status();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Foreground daemon; returns once its banner says it is listening.
fn start_daemon(vault: &std::path::Path) -> Daemon {
    let mut child = bin()
        .arg("--vault")
        .arg(vault)
        .arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).expect("daemon banner");
    let banner: Value = serde_json::from_str(line.trim()).unwrap_or_else(|_| {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut e, &mut err);
        }
        panic!("bad banner: {line:?} stderr: {err}")
    });
    assert_eq!(banner["event"], "listening");
    Daemon(child)
}

fn daemon_running(vault: &std::path::Path) -> bool {
    let out = run_ok(vault, &["status", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    v["daemon"] == json!(true)
}

#[test]
fn daemon_start_takes_over_from_embedded_session_which_becomes_a_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    init_vault(vault);

    let mut session = Session::open(vault);
    assert_eq!(session.status()["agent"]["id"], "solo");
    assert!(vault.join(".tusk/lock").exists());

    // This is the call that used to fail with "vault is locked by another
    // process" for as long as the client lived.
    let daemon = start_daemon(vault);
    assert!(daemon_running(vault));

    // The client's session survived the handover and is now served by the
    // daemon: the daemon's own status reports the agent as connected.
    let before = session.status();
    assert_eq!(before["agent"]["id"], "solo");
    let status = run_ok(vault, &["status", "--json"]);
    let v: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(v["daemon"], json!(true));

    // Stdin close still ends the session promptly (build-loop §3.1).
    session.close_stdin();
    assert!(session.wait_exit(2).success());
    drop(daemon);
}

#[test]
fn one_shot_commands_borrow_the_vault_from_an_embedded_session() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    init_vault(vault);

    let mut session = Session::open(vault);
    session.status();

    // Used to fail outright; now the session yields for the duration.
    let out = run_ok(vault, &["status", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["daemon"], json!(false));

    // …and takes the vault back afterwards: the session still works.
    assert_eq!(session.status()["agent"]["id"], "solo");
    assert_eq!(session.status()["agent"]["id"], "solo");

    session.close_stdin();
    assert!(session.wait_exit(2).success());
}

#[test]
fn second_embedded_session_still_refuses_and_names_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    init_vault(vault);

    let mut first = Session::open(vault);
    let first_pid = first.child.id();

    // Two embedded sessions must never ping-pong the lock: the second one
    // fails fast, and its error says who has the vault.
    let out = bin()
        .arg("--vault")
        .arg(vault)
        .args(["mcp", "--agent", "solo"])
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("vault is locked by another process"),
        "{stderr}"
    );
    assert!(stderr.contains(&format!("pid {first_pid}")), "{stderr}");
    assert!(stderr.contains("mcp --agent solo"), "{stderr}");

    // The first session was not disturbed.
    assert_eq!(first.status()["agent"]["id"], "solo");
    first.close_stdin();
    first.wait_exit(2);
}

#[test]
fn stale_handoff_request_does_not_wedge_an_embedded_session() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    init_vault(vault);

    // A requester that died after asking. The session must notice nobody
    // took the vault, clean up, and keep serving.
    std::fs::write(vault.join(".tusk/handoff"), "999999 test\n").unwrap();
    let mut session = Session::open(vault);
    let t0 = Instant::now();
    assert_eq!(session.status()["agent"]["id"], "solo");
    assert!(t0.elapsed() < Duration::from_secs(15));
    // Give the session a moment to notice and tidy up.
    let deadline = Instant::now() + Duration::from_secs(15);
    while vault.join(".tusk/handoff").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !vault.join(".tusk/handoff").exists(),
        "stale request not cleaned up"
    );
    assert_eq!(session.status()["agent"]["id"], "solo");
    session.close_stdin();
    assert!(session.wait_exit(2).success());
}

#[test]
fn proxied_session_falls_back_to_embedded_when_the_daemon_stops_and_back_again() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    init_vault(vault);

    let daemon = start_daemon(vault);
    let mut session = Session::open(vault); // proxying
    assert_eq!(session.status()["agent"]["id"], "solo");

    // `tuskd stop` must succeed even though the session takes the vault
    // over the instant the daemon lets go.
    let out = run(vault, &["stop"]);
    assert!(
        out.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(daemon);

    // The client never saw its MCP server die.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s = session.status();
        if s["agent"]["id"] == "solo" {
            break;
        }
        assert!(Instant::now() < deadline, "session did not recover: {s}");
    }
    assert!(!daemon_running(vault));

    // And a new daemon takes it back once more.
    let daemon2 = start_daemon(vault);
    assert!(daemon_running(vault));
    assert_eq!(session.status()["agent"]["id"], "solo");

    session.close_stdin();
    assert!(session.wait_exit(2).success());
    drop(daemon2);
}

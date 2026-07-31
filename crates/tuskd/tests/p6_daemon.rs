//! P6 exit tests — daemon, transports, CLI (build-loop §2 P6).
//! These drive the real `tuskd` binary.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tuskd"))
}

/// Run a one-shot CLI command in `vault`, asserting success; returns stdout.
fn run_ok(vault: &std::path::Path, args: &[&str]) -> String {
    let out = bin()
        .arg("--vault")
        .arg(vault)
        .args(args)
        .output()
        .expect("spawn tuskd");
    assert!(
        out.status.success(),
        "tuskd {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn create_agent(vault: &std::path::Path, id: &str, extra: &[&str]) -> String {
    let stdout = run_ok(vault, &[&["agent", "create", id], extra].concat());
    // Token line: "token: tusk_..."
    let token = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("token: "))
        .unwrap_or_else(|| panic!("no token in output: {stdout}"))
        .to_string();
    assert!(token.starts_with("tusk_"));
    token
}

struct Daemon {
    child: Child,
    http: String,
    #[allow(dead_code)]
    stdout: BufReader<std::process::ChildStdout>,
}

/// If a test panics before SIGTERM, kill the daemon so it can't outlive the
/// test run (it would otherwise hold inherited pipes open and wedge cargo).
impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn start_daemon(vault: &std::path::Path) -> Daemon {
    // Ephemeral port so parallel tests never collide; the banner reports it.
    let cfg_path = vault.join(".tusk").join("tuskd.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    std::fs::write(&cfg_path, cfg.replace("http_port = 7477", "http_port = 0")).unwrap();
    let mut child = bin()
        .arg("--vault")
        .arg(vault)
        .arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).expect("daemon banner");
    let banner: Value =
        serde_json::from_str(line.trim()).unwrap_or_else(|_| panic!("bad banner: {line}"));
    assert_eq!(banner["event"], "listening");
    let http = banner["http"].as_str().unwrap().to_string();
    Daemon {
        child,
        http,
        stdout,
    }
}

fn sigterm(child: &Child) {
    let _ = Command::new("kill")
        .arg(child.id().to_string())
        .status()
        .expect("kill");
}

/// Wait for exit, asserting it happens within `secs` seconds.
fn wait_exit(child: &mut Child, secs: u64) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit within {secs}s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn mcp_post(
    client: &reqwest::blocking::Client,
    url: &str,
    token: &str,
    body: Value,
) -> (u16, Value) {
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .expect("http post");
    let status = resp.status().as_u16();
    let value = resp.json::<Value>().unwrap_or(Value::Null);
    (status, value)
}

fn init_msg() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
           "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                      "clientInfo": {"name": "p6-test", "version": "0"}}})
}

fn tool_call(id: i64, name: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
           "params": {"name": name, "arguments": args}})
}

/// Extract the text content of an MCP tool result and parse it as JSON.
fn tool_json(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {resp}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("non-JSON tool text ({e}): {text}"))
}

#[test]
fn daemon_status_mcp_http_and_clean_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    run_ok(vault, &["init"]);
    let token = create_agent(
        vault,
        "hermes-dev",
        &["--write", "project:opentusk", "--read", "project:opentusk"],
    );

    let mut daemon = start_daemon(vault);
    let client = reqwest::blocking::Client::new();

    // /status 200 with stats.
    let resp = client
        .get(format!("http://{}/status", daemon.http))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let status: Value = resp.json().unwrap();
    assert!(status["index"]["total"].is_number());
    assert!(status["version"].is_string());

    let mcp_url = format!("http://{}/mcp", daemon.http);
    // 401 on bad token.
    let (code, _) = mcp_post(&client, &mcp_url, "tusk_wrong", init_msg());
    assert_eq!(code, 401);
    // Unauthenticated too.
    let resp = client.post(&mcp_url).json(&init_msg()).send().unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    // Good token: initialize handshake.
    let (code, resp) = mcp_post(&client, &mcp_url, &token, init_msg());
    assert_eq!(code, 200);
    assert_eq!(resp["result"]["serverInfo"]["name"], "tuskd");
    assert!(resp["result"]["protocolVersion"].is_string());

    // tools/list then a tool call.
    let (_, resp) = mcp_post(
        &client,
        &mcp_url,
        &token,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    assert_eq!(resp["result"]["tools"].as_array().unwrap().len(), 9);

    let (_, resp) = mcp_post(
        &client,
        &mcp_url,
        &token,
        tool_call(
            3,
            "memory_write",
            json!({"content": "daemon-written zebrafish note", "scope": "project:opentusk", "type": "semantic"}),
        ),
    );
    assert_eq!(resp["result"]["isError"], false);
    let id = tool_json(&resp)["id"].as_str().unwrap().to_string();
    assert!(!id.is_empty());

    // CLI one-shot routed through the live daemon (D6): search finds it.
    let cli_out = run_ok(
        vault,
        &["search", "zebrafish", "--scope", "project:opentusk"],
    );
    assert!(cli_out.contains(&id), "CLI search must hit: {cli_out}");

    // SIGTERM → exit < 2s, lock released.
    sigterm(&daemon.child);
    let status = wait_exit(&mut daemon.child, 2);
    let _ = status;
    // Lock released: an embedded one-shot command succeeds again.
    run_ok(vault, &["status"]);
}

#[test]
fn daemon_restart_preserves_search_results() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    run_ok(vault, &["init"]);
    let token = create_agent(
        vault,
        "writer",
        &["--write", "project:opentusk", "--read", "project:opentusk"],
    );

    let client = reqwest::blocking::Client::new();
    let mut daemon = start_daemon(vault);
    let mcp_url = format!("http://{}/mcp", daemon.http);
    let (_, resp) = mcp_post(
        &client,
        &mcp_url,
        &token,
        tool_call(
            1,
            "memory_write",
            json!({"content": "persistent quasar knowledge", "scope": "project:opentusk", "type": "semantic"}),
        ),
    );
    let id = tool_json(&resp)["id"].as_str().unwrap().to_string();
    let (_, resp) = mcp_post(
        &client,
        &mcp_url,
        &token,
        tool_call(
            2,
            "memory_search",
            json!({"query": "quasar", "scopes": ["project:opentusk"]}),
        ),
    );
    let hits1 = tool_json(&resp)["hits"].as_array().unwrap().clone();
    assert_eq!(hits1.len(), 1);

    sigterm(&daemon.child);
    wait_exit(&mut daemon.child, 2);

    let mut daemon2 = start_daemon(vault);
    let mcp_url = format!("http://{}/mcp", daemon2.http);
    let (_, resp) = mcp_post(
        &client,
        &mcp_url,
        &token,
        tool_call(
            3,
            "memory_search",
            json!({"query": "quasar", "scopes": ["project:opentusk"]}),
        ),
    );
    let hits2 = tool_json(&resp)["hits"].as_array().unwrap().clone();
    assert_eq!(hits2.len(), 1);
    assert_eq!(hits2[0]["id"].as_str().unwrap(), id);
    assert_eq!(hits1[0]["id"], hits2[0]["id"]);

    sigterm(&daemon2.child);
    wait_exit(&mut daemon2.child, 2);
}

#[test]
fn stdio_embedded_session_exits_within_2s_of_stdin_close() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    run_ok(vault, &["init"]);
    create_agent(vault, "solo", &[]);

    let mut child = bin()
        .arg("--vault")
        .arg(vault)
        .args(["mcp", "--agent", "solo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(stdin, "{}", init_msg()).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let resp: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp["result"]["serverInfo"]["name"], "tuskd");

    // Initialized notification (no response expected).
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();

    // One tool call.
    writeln!(stdin, "{}", tool_call(2, "memory_status", json!({}))).unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    let resp: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp["result"]["isError"], false);
    let status = tool_json(&resp);
    assert_eq!(status["agent"]["id"], "solo");

    // The TS prototype's lingering-child bug: must exit within 2s of
    // stdin close (build-loop §3.1).
    drop(stdin);
    let status = wait_exit(&mut child, 2);
    assert!(status.success());
}

#[test]
fn second_embedded_instance_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    run_ok(vault, &["init"]);
    create_agent(vault, "solo", &[]);

    let mut first = bin()
        .arg("--vault")
        .arg(vault)
        .args(["mcp", "--agent", "solo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin1 = first.stdin.take().unwrap();
    let mut stdout1 = BufReader::new(first.stdout.take().unwrap());
    writeln!(stdin1, "{}", init_msg()).unwrap();
    let mut line = String::new();
    stdout1.read_line(&mut line).unwrap(); // first session is live and holds the lock

    let out = bin()
        .arg("--vault")
        .arg(vault)
        .args(["mcp", "--agent", "solo"])
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "second embedded instance must refuse to start"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("lock"),
        "error should mention the lock: {stderr}"
    );

    drop(stdin1);
    wait_exit(&mut first, 2);
}

#[test]
fn stdio_refuses_unknown_and_revoked_agent() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    run_ok(vault, &["init"]);
    create_agent(vault, "real", &[]);
    run_ok(vault, &["agent", "revoke", "real"]);

    for agent in ["ghost", "real"] {
        let out = bin()
            .arg("--vault")
            .arg(vault)
            .args(["mcp", "--agent", agent])
            .stdin(Stdio::piped())
            .output()
            .unwrap();
        assert!(!out.status.success(), "agent {agent} must be refused");
    }
}

#[test]
fn cli_review_and_graduate_flow_embedded() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    run_ok(vault, &["init"]);
    create_agent(
        vault,
        "hermes-dev",
        &["--write", "project:opentusk", "--read", "project:opentusk"],
    );

    // Write a procedural record and give it winning telemetry via stdio session.
    let mut child = bin()
        .arg("--vault")
        .arg(vault)
        .args(["mcp", "--agent", "hermes-dev"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    writeln!(stdin, "{}", init_msg()).unwrap();
    stdout.read_line(&mut line).unwrap();

    writeln!(
        stdin,
        "{}",
        tool_call(2, "memory_write", json!({"content": "Run scripts/envdiff before every deploy.", "type": "procedural", "scope": "project:opentusk"}))
    )
    .unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    let resp: Value = serde_json::from_str(line.trim()).unwrap();
    let id = tool_json(&resp)["id"].as_str().unwrap().to_string();
    for i in 0..7 {
        writeln!(
            stdin,
            "{}",
            tool_call(
                10 + i,
                "memory_feedback",
                json!({"id": &id, "outcome": "success"})
            )
        )
        .unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
    }
    drop(stdin);
    wait_exit(&mut child, 2);

    // graduate → one queue item; review list shows it; approve materializes.
    let out = run_ok(vault, &["graduate"]);
    assert!(out.contains("queued"), "graduate output: {out}");
    let list = run_ok(vault, &["review", "list"]);
    let qid = list
        .lines()
        .find_map(|l| l.split_whitespace().find(|w| w.starts_with("q-")))
        .unwrap_or_else(|| panic!("no qid in review list: {list}"))
        .to_string();
    let approved = run_ok(vault, &["review", "approve", &qid]);
    assert!(approved.contains("committed"), "approve output: {approved}");

    let skills_root = vault.join("skills").join("project-opentusk");
    let entries: Vec<_> = std::fs::read_dir(&skills_root).unwrap().collect();
    assert_eq!(entries.len(), 1);
    let skill_md = entries[0].as_ref().unwrap().path().join("SKILL.md");
    let text = std::fs::read_to_string(skill_md).unwrap();
    assert!(text.contains("name: "));
    assert!(text.contains("description: "));
}

// ---------------------------------------------------------------------------
// D32: default-port fallback + surfaced boot errors
// ---------------------------------------------------------------------------

/// The daemon's bind logic is exercised through a full boot: a vault whose
/// configured port equals the (test-chosen) "default" must fall back to an
/// ephemeral port when that port is taken; a custom port must fail hard.
/// We can't test literal 7477 (fixed ports are forbidden in tests), so this
/// covers the helper's contract via the daemon module's unit surface.
#[tokio::test]
async fn default_port_falls_back_but_custom_port_fails_hard() {
    // Occupy an ephemeral port to simulate "someone owns the default".
    let occupier = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("occupy");
    let busy = occupier.local_addr().expect("addr").port();

    // Busy + it IS the default → ephemeral fallback.
    let (tcp, fell_back) = tuskd::daemon::bind_http(busy, busy)
        .await
        .expect("fallback");
    assert!(fell_back);
    assert_ne!(tcp.local_addr().expect("addr").port(), busy);

    // Busy + custom (default is some other port) → hard error naming the addr.
    let err = tuskd::daemon::bind_http(busy, busy.wrapping_add(1))
        .await
        .expect_err("custom port must fail hard");
    assert!(err.to_string().contains(&format!("127.0.0.1:{busy}")));

    // Free port → no fallback. Another test (they run in parallel) can
    // grab the port in the drop→rebind window, so retry on a fresh
    // ephemeral port instead of asserting on one racy attempt.
    drop(occupier);
    for attempt in 0.. {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("probe");
        let port = probe.local_addr().expect("addr").port();
        drop(probe);
        let (_tcp, fell_back) = tuskd::daemon::bind_http(port, port).await.expect("rebind");
        if !fell_back {
            break;
        }
        assert!(attempt < 5, "no free port stayed free across 5 attempts");
    }
}

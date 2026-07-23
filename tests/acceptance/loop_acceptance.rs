//! Acceptance Suite — the 11-step loop from tuskd-build-loop.md §4, run once
//! over embedded stdio and once over HTTP against a live daemon, driving the
//! real `tuskd` binary end to end.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tuskd"))
}

fn run_cli(vault: &Path, args: &[&str]) -> String {
    let out = bin().arg("--vault").arg(vault).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "tuskd {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Fresh temp vault with the two acceptance agents (build-loop §4).
/// hermes-dev: read project:opentusk,user; promote project:opentusk.
/// claude-code: read project:opentusk; promote project:opentusk.
fn setup_vault() -> (tempfile::TempDir, Tokens) {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path();
    run_cli(vault, &["init"]);
    let hermes = create_agent(
        vault,
        "hermes-dev",
        &[
            "--read",
            "project:opentusk,user",
            "--promote",
            "project:opentusk",
        ],
    );
    let claude = create_agent(
        vault,
        "claude-code",
        &[
            "--read",
            "project:opentusk",
            "--promote",
            "project:opentusk",
        ],
    );
    (dir, Tokens { hermes, claude })
}

struct Tokens {
    hermes: String,
    claude: String,
}

fn create_agent(vault: &Path, id: &str, extra: &[&str]) -> String {
    let stdout = run_cli(vault, &[&["agent", "create", id], extra].concat());
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("token: "))
        .unwrap_or_else(|| panic!("no token in output: {stdout}"))
        .to_string()
}

/// One tool call: (is_error, parsed-or-raw text).
struct CallResult {
    is_error: bool,
    text: String,
}

impl CallResult {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("non-JSON tool text ({e}): {}", self.text))
    }
}

trait Transport {
    fn call(&self, agent: &str, tool: &str, args: Value) -> CallResult;
}

/// Embedded stdio: a fresh `opentusk mcp --agent <id>` process per call —
/// sequential single-user sessions, each owning the vault lock briefly.
struct StdioTransport {
    vault: PathBuf,
}

impl Transport for StdioTransport {
    fn call(&self, agent: &str, tool: &str, args: Value) -> CallResult {
        let mut child = bin()
            .arg("--vault")
            .arg(&self.vault)
            .args(["mcp", "--agent", agent])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                              "clientInfo": {"name": "acceptance", "version": "0"}}})
        )
        .unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": tool, "arguments": args}})
        )
        .unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("bad stdio response ({e}): {line}"));
        drop(stdin);
        wait_exit(&mut child, 2);
        extract(&resp)
    }
}

/// HTTP against a live daemon: bearer token per agent, one POST per call.
struct HttpTransport {
    url: String,
    hermes: String,
    claude: String,
}

impl Transport for HttpTransport {
    fn call(&self, agent: &str, tool: &str, args: Value) -> CallResult {
        let token = match agent {
            "hermes-dev" => &self.hermes,
            "claude-code" => &self.claude,
            other => panic!("unknown agent {other}"),
        };
        let client = reqwest::blocking::Client::new();
        let resp = client
            .post(&self.url)
            .bearer_auth(token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": tool, "arguments": args}}))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "tools/call must be 200");
        let resp: Value = resp.json().unwrap();
        extract(&resp)
    }
}

fn extract(resp: &Value) -> CallResult {
    let result = &resp["result"];
    assert!(
        !result.is_null(),
        "expected result, got error response: {resp}"
    );
    CallResult {
        is_error: result["isError"].as_bool().unwrap_or(false),
        text: result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

fn wait_exit(child: &mut Child, secs: u64) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit within {secs}s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The 11 steps (same assertions for both transports).
fn run_loop_steps(vault: &Path, t: &dyn Transport) {
    // 1. hermes writes a private episodic memory → returns id.
    let r = t.call(
        "hermes-dev",
        "memory_write",
        json!({"content": "Deploy to staging failed at 14:02 — missing env var WALRUS_EPOCHS",
               "type": "episodic"}),
    );
    assert!(!r.is_error, "step 1: {}", r.text);
    let episodic_id = r.json()["id"].as_str().unwrap().to_string();
    assert!(!episodic_id.is_empty());

    // 2. claude searches hermes' private scope → DENIED.
    let r = t.call(
        "claude-code",
        "memory_search",
        json!({"query": "WALRUS_EPOCHS", "scopes": ["agent:hermes-dev"]}),
    );
    assert!(r.is_error, "step 2 must be denied");
    assert!(r.text.starts_with("DENIED"), "step 2: {}", r.text);

    // 3. hermes reflects two candidates into project:opentusk → both committed.
    let fact_body =
        "Staging and production environments must maintain env var parity, including WALRUS_EPOCHS.";
    let proc_body = "Run scripts/envdiff before every deploy to verify env parity.";
    let r = t.call(
        "hermes-dev",
        "memory_reflect",
        json!({"candidates": [
            {"type": "fact", "content": fact_body, "scope": "project:opentusk"},
            {"type": "procedure", "content": proc_body, "scope": "project:opentusk"}
        ]}),
    );
    assert!(!r.is_error, "step 3: {}", r.text);
    let results = r.json()["results"].as_array().unwrap().clone();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]["action"], "committed",
        "step 3 fact: {results:?}"
    );
    assert_eq!(
        results[1]["action"], "committed",
        "step 3 procedure: {results:?}"
    );
    let fact_id = results[0]["id"].as_str().unwrap().to_string();
    let proc_id = results[1]["id"].as_str().unwrap().to_string();

    // Timestamp between step 3 and step 7 for the as_of probe (step 8).
    std::thread::sleep(Duration::from_millis(30));
    let t_mid = chrono::Utc::now().to_rfc3339();
    std::thread::sleep(Duration::from_millis(30));

    // 4. hermes promotes the identical procedural text → rejected_duplicate.
    let r = t.call(
        "hermes-dev",
        "memory_promote",
        json!({"content": proc_body, "type": "procedure", "target_scope": "project:opentusk"}),
    );
    assert!(!r.is_error, "step 4: {}", r.text);
    let out = r.json();
    assert_eq!(out["action"], "rejected_duplicate", "step 4: {out}");
    assert_eq!(out["existing_id"], *proc_id);

    // 5. claude searches "deploy env parity" in project:opentusk → hit
    //    includes the procedure id.
    let r = t.call(
        "claude-code",
        "memory_search",
        json!({"query": "deploy env parity", "scopes": ["project:opentusk"]}),
    );
    assert!(!r.is_error, "step 5: {}", r.text);
    let hits = r.json()["hits"].as_array().unwrap().clone();
    assert!(
        hits.iter().any(|h| h["id"] == *proc_id),
        "step 5: procedure {proc_id} not in hits: {hits:?}"
    );

    // 6. 6× claude + 1× hermes feedback success → uses=7, success_rate=1.0.
    for i in 0..7 {
        let agent = if i < 6 { "claude-code" } else { "hermes-dev" };
        let r = t.call(
            agent,
            "memory_feedback",
            json!({"id": &proc_id, "outcome": "success"}),
        );
        assert!(!r.is_error, "step 6 ({agent}): {}", r.text);
        if i == 6 {
            let out = r.json();
            assert_eq!(out["uses"], 7, "step 6: {out}");
            assert_eq!(out["success_rate"], 1.0, "step 6: {out}");
        }
    }

    // 7. hermes promotes a correction with corrects=<fact-id> →
    //    superseded_existing.
    let correction_body = "Correction: staging env parity must also cover WALRUS_RETENTION, \
                           not only WALRUS_EPOCHS — verified against production config.";
    let r = t.call(
        "hermes-dev",
        "memory_promote",
        json!({"content": correction_body, "type": "correction",
               "target_scope": "project:opentusk", "corrects": &fact_id}),
    );
    assert!(!r.is_error, "step 7: {}", r.text);
    let out = r.json();
    assert_eq!(out["action"], "superseded_existing", "step 7: {out}");
    assert_eq!(out["superseded"], *fact_id);
    let correction_id = out["id"].as_str().unwrap().to_string();

    // 8. as_of = t_mid → the ORIGINAL fact; at now → the correction only.
    let r = t.call(
        "claude-code",
        "memory_search",
        json!({"query": "env parity", "scopes": ["project:opentusk"], "as_of": &t_mid}),
    );
    assert!(!r.is_error, "step 8a: {}", r.text);
    let hits = r.json()["hits"].as_array().unwrap().clone();
    let ids: Vec<&str> = hits.iter().filter_map(|h| h["id"].as_str()).collect();
    assert!(ids.contains(&fact_id.as_str()), "step 8a: {ids:?}");
    assert!(!ids.contains(&correction_id.as_str()), "step 8a: {ids:?}");

    let r = t.call(
        "claude-code",
        "memory_search",
        json!({"query": "env parity", "scopes": ["project:opentusk"]}),
    );
    let hits = r.json()["hits"].as_array().unwrap().clone();
    let ids: Vec<&str> = hits.iter().filter_map(|h| h["id"].as_str()).collect();
    assert!(ids.contains(&correction_id.as_str()), "step 8b: {ids:?}");
    assert!(!ids.contains(&fact_id.as_str()), "step 8b: {ids:?}");

    // 9. memory_status → totals ≥ 4, grants echoed, queue depth correct.
    let r = t.call("hermes-dev", "memory_status", json!({}));
    assert!(!r.is_error, "step 9: {}", r.text);
    let status = r.json();
    assert!(
        status["index"]["total"].as_i64().unwrap() >= 4,
        "step 9: {status}"
    );
    assert!(
        status["agent"]["grants"]["read"]
            .as_array()
            .unwrap()
            .contains(&json!("project:opentusk")),
        "step 9 grants: {status}"
    );
    assert_eq!(status["review_queue_depth"], 0, "step 9: {status}");

    // 10. opentusk graduate → exactly one queue item, type=skill, tag
    //     graduated.
    let out = run_cli(vault, &["graduate"]);
    assert!(out.contains("queued"), "step 10: {out}");
    let list = run_cli(vault, &["review", "list"]);
    let qids: Vec<&str> = list
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| w.starts_with("q-"))
        .collect();
    assert_eq!(qids.len(), 1, "step 10: exactly one queue item: {list}");
    assert!(list.contains("skill"), "step 10 type=skill: {list}");
    let qid = qids[0].to_string();
    // Tag `graduated` is on the queued candidate.
    let r = t.call("hermes-dev", "memory_status", json!({}));
    assert_eq!(r.json()["review_queue_depth"], 1, "step 10 queue depth");
    let queue_raw =
        std::fs::read_to_string(vault.join(".tusk").join("queue").join("review.json")).unwrap();
    assert!(queue_raw.contains("graduated"), "step 10 tag: {queue_raw}");

    // 11. review approve → skill committed AND SKILL.md materialized with
    //     name: and description: frontmatter.
    let out = run_cli(vault, &["review", "approve", &qid]);
    assert!(out.contains("committed"), "step 11: {out}");
    let skill_id = serde_json::from_str::<Value>(&out).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let skill_md = vault
        .join("skills")
        .join("project-opentusk")
        .join(&skill_id)
        .join("SKILL.md");
    assert!(skill_md.exists(), "step 11: {} missing", skill_md.display());
    let text = std::fs::read_to_string(&skill_md).unwrap();
    assert!(text.contains("name: "), "step 11 frontmatter: {text}");
    assert!(
        text.contains("description: "),
        "step 11 frontmatter: {text}"
    );

    // Plus: index rebuild then repeat step 5 → identical hit.
    run_cli(vault, &["index", "rebuild"]);
    let r = t.call(
        "claude-code",
        "memory_search",
        json!({"query": "deploy env parity", "scopes": ["project:opentusk"]}),
    );
    let hits = r.json()["hits"].as_array().unwrap().clone();
    assert!(
        hits.iter().any(|h| h["id"] == *proc_id),
        "post-rebuild: procedure lost: {hits:?}"
    );
}

#[test]
fn acceptance_loop_embedded_stdio() {
    let (dir, _tokens) = setup_vault();
    let transport = StdioTransport {
        vault: dir.path().to_path_buf(),
    };
    run_loop_steps(dir.path(), &transport);
}

#[test]
fn acceptance_loop_http_daemon() {
    let (dir, tokens) = setup_vault();
    let vault = dir.path();

    // Ephemeral port to avoid collisions.
    let cfg_path = vault.join(".tusk").join("opentusk.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    std::fs::write(&cfg_path, cfg.replace("http_port = 7477", "http_port = 0")).unwrap();

    let mut child = bin()
        .arg("--vault")
        .arg(vault)
        .arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let banner: Value = serde_json::from_str(line.trim()).unwrap();
    let http = banner["http"].as_str().unwrap().to_string();

    // Kill-on-drop guard so a failed assertion can't orphan the daemon.
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let mut guard = Guard(child);

    // Plus: unauthorized /mcp → 401.
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("http://{http}/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let transport = HttpTransport {
        url: format!("http://{http}/mcp"),
        hermes: tokens.hermes,
        claude: tokens.claude,
    };
    run_loop_steps(vault, &transport);

    // Plus: daemon SIGTERM → exits < 2s, lock released.
    let _ = Command::new("kill")
        .arg(guard.0.id().to_string())
        .status()
        .unwrap();
    wait_exit(&mut guard.0, 2);
    run_cli(vault, &["status"]); // lock is free again
}

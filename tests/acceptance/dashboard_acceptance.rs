//! Dashboard acceptance — drives the real `tuskd` binary end to end:
//! operator token auth, the `/api/admin` bridge, the embedded `/ui` shell,
//! memories/review housekeeping, export download, and `tuskd dashboard`.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::Path;
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

/// Init a vault and switch it to an ephemeral HTTP port.
fn setup_vault() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    run_cli(dir.path(), &["init"]);
    let cfg_path = dir.path().join(".tusk").join("tuskd.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    std::fs::write(&cfg_path, cfg.replace("http_port = 7477", "http_port = 0")).unwrap();
    dir
}

/// Kill-on-drop daemon guard (a failed assertion must not orphan the daemon).
struct Daemon {
    child: Child,
    http: String,
    dashboard_url: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn start_daemon(vault: &Path) -> Daemon {
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
    Daemon {
        child,
        http: banner["http"].as_str().unwrap().to_string(),
        dashboard_url: banner["dashboard"].as_str().unwrap().to_string(),
    }
}

fn wait_exit(child: &mut Child, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "daemon did not exit in {secs}s");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The operator token file the daemon writes at start (JSON: url/http/token).
fn read_token_file(vault: &Path) -> Value {
    let path = vault.join(".tusk").join("admin-token");
    let raw = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::new()
}

/// POST /api/admin with the operator token.
fn api_admin(base: &str, token: &str, body: Value) -> Value {
    let resp = client()
        .post(format!("http://{base}/api/admin"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "api/admin must be 200");
    resp.json().unwrap()
}

fn api_admin_ok(base: &str, token: &str, body: Value) -> Value {
    let resp = api_admin(base, token, body.clone());
    assert_eq!(resp["ok"], true, "admin {body} failed: {resp}");
    resp["data"].clone()
}

/// One MCP tools/call over HTTP as an agent; returns the parsed tool text.
fn mcp_call(base: &str, agent_token: &str, tool: &str, args: Value) -> Value {
    let resp = client()
        .post(format!("http://{base}/mcp"))
        .bearer_auth(agent_token)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": tool, "arguments": args}}))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let resp: Value = resp.json().unwrap();
    let result = &resp["result"];
    assert!(
        !result["isError"].as_bool().unwrap_or(false),
        "tool {tool} errored: {resp}"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

/// P8: operator token issued + 0600, /ui shell served, /api/admin auth
/// enforced, status + search parity through the bridge.
#[test]
fn dashboard_auth_status_search() {
    let dir = setup_vault();
    let vault = dir.path();
    let d = start_daemon(vault);
    let base = &d.http;

    // Banner advertises the dashboard URL with the token in the fragment.
    assert!(
        d.dashboard_url.contains("/ui/#t=tuskop_"),
        "banner dashboard url: {}",
        d.dashboard_url
    );

    // Token file: matches the banner, and is private (0600).
    let tok = read_token_file(vault);
    let token = tok["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("tuskop_"), "token: {token}");
    assert_eq!(tok["url"].as_str().unwrap(), d.dashboard_url);
    assert_eq!(tok["http"].as_str().unwrap(), base.as_str());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(vault.join(".tusk").join("admin-token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "admin-token must be 0600");
    }

    // /ui serves the embedded shell without auth (it contains no data).
    for path in ["/ui", "/ui/"] {
        let resp = client().get(format!("http://{base}{path}")).send().unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{path}");
        let text = resp.text().unwrap();
        assert!(text.contains("OpenTusk"), "{path} is not the dashboard");
    }

    // /api/* rejects missing and wrong tokens.
    let resp = client()
        .post(format!("http://{base}/api/admin"))
        .json(&json!({"cmd": "status"}))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "no token must be 401");
    let resp = client()
        .post(format!("http://{base}/api/admin"))
        .bearer_auth("tuskop_wrong")
        .json(&json!({"cmd": "status"}))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "bad token must be 401");
    // An *agent* token is not an operator token.
    let agent = api_admin_ok(base, &token, json!({"cmd": "agent_create", "id": "probe"}));
    let agent_token = agent["token"].as_str().unwrap().to_string();
    let resp = client()
        .post(format!("http://{base}/api/admin"))
        .bearer_auth(&agent_token)
        .json(&json!({"cmd": "status"}))
        .send()
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "agent token must not pass /api"
    );

    // Status through the bridge has the admin shape.
    let status = api_admin_ok(base, &token, json!({"cmd": "status"}));
    assert!(status["version"].is_string());
    assert!(status["index"]["total"].is_number());
    assert!(status["review_queue_depth"].is_number());

    // /api/meta: uptime + identity; auth required.
    let resp = client()
        .get(format!("http://{base}/api/meta"))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let meta: Value = client()
        .get(format!("http://{base}/api/meta"))
        .bearer_auth(&token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(meta["uptime_secs"].is_number());
    assert!(meta["version"].is_string());
    assert!(meta["vault"].is_string());

    // /api/config: effective config; auth required.
    let resp = client()
        .get(format!("http://{base}/api/config"))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let cfg: Value = client()
        .get(format!("http://{base}/api/config"))
        .bearer_auth(&token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(cfg["http_port"], 0);
    assert!(cfg["graduation"]["min_uses"].is_number());

    // Search parity: write through MCP as the agent, find through the bridge.
    let rec = mcp_call(
        base,
        &agent_token,
        "memory_write",
        json!({"content": "dashboard probe memory: quokka epsilon"}),
    );
    let rec_id = rec["id"].as_str().unwrap();
    let found = api_admin_ok(base, &token, json!({"cmd": "search", "query": "quokka"}));
    assert_eq!(found["count"], 1, "search through bridge: {found}");
    assert_eq!(found["hits"][0]["id"].as_str().unwrap(), rec_id);
}

/// P9: record browse/get/forget through the bridge, and the review loop.
#[test]
fn dashboard_memories_and_review() {
    let dir = setup_vault();
    let vault = dir.path();
    let d = start_daemon(vault);
    let base = &d.http;
    let token = read_token_file(vault)["token"]
        .as_str()
        .unwrap()
        .to_string();

    // An agent with promote on org (org policy defaults to review).
    let agent = api_admin_ok(
        base,
        &token,
        json!({"cmd": "agent_create", "id": "curator", "promote": ["org"]}),
    );
    let agent_token = agent["token"].as_str().unwrap().to_string();

    // Two private records to browse.
    let a = mcp_call(
        base,
        &agent_token,
        "memory_write",
        json!({"content": "browse target alpha: xylophone"}),
    );
    let b = mcp_call(
        base,
        &agent_token,
        "memory_write",
        json!({"content": "browse target beta: marimba", "type": "semantic"}),
    );
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    // record_list: everything, newest first.
    let list = api_admin_ok(base, &token, json!({"cmd": "record_list"}));
    assert_eq!(list["count"], 2, "record_list: {list}");
    // scope + type filters.
    let list = api_admin_ok(
        base,
        &token,
        json!({"cmd": "record_list", "scope": "agent:curator", "kind": "semantic"}),
    );
    assert_eq!(list["count"], 1);
    assert_eq!(list["records"][0]["id"].as_str().unwrap(), b_id);
    // limit + offset paginate.
    let page = api_admin_ok(base, &token, json!({"cmd": "record_list", "limit": 1}));
    assert_eq!(page["count"], 1);
    let page2 = api_admin_ok(
        base,
        &token,
        json!({"cmd": "record_list", "limit": 1, "offset": 1}),
    );
    assert_eq!(page2["count"], 1);
    assert_ne!(
        page["records"][0]["id"], page2["records"][0]["id"],
        "offset must advance"
    );

    // record_get returns the full record including the body.
    let got = api_admin_ok(base, &token, json!({"cmd": "record_get", "id": a_id}));
    assert!(got["body"].as_str().unwrap().contains("xylophone"));
    assert_eq!(got["scope"], "agent:curator");

    // forget removes file + index row.
    api_admin_ok(base, &token, json!({"cmd": "forget", "id": a_id}));
    let resp = api_admin(base, &token, json!({"cmd": "record_get", "id": a_id}));
    assert_eq!(resp["ok"], false, "forgotten record must be gone");
    let found = api_admin_ok(base, &token, json!({"cmd": "search", "query": "xylophone"}));
    assert_eq!(found["count"], 0);

    // Review loop: promote to org queues, approve commits.
    let queued = mcp_call(
        base,
        &agent_token,
        "memory_promote",
        json!({"content": "org-wide fact: deploys need envdiff", "target_scope": "org"}),
    );
    assert_eq!(queued["action"], "queued", "promote to org: {queued}");
    let items = api_admin_ok(base, &token, json!({"cmd": "review_list"}));
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 1);
    let qid = items[0]["qid"].as_str().unwrap().to_string();
    let outcome = api_admin_ok(
        base,
        &token,
        json!({"cmd": "review", "qid": qid, "approve": true}),
    );
    let new_id = outcome["id"].as_str().unwrap();
    let rec = api_admin_ok(base, &token, json!({"cmd": "record_get", "id": new_id}));
    assert_eq!(rec["scope"], "org");
    let depth = api_admin_ok(base, &token, json!({"cmd": "status"}));
    assert_eq!(depth["review_queue_depth"], 0);
}

/// P10: export download, one-click housekeeping, `tuskd dashboard`, and
/// clean shutdown removing the operator token file.
#[test]
fn dashboard_housekeeping_and_command() {
    let dir = setup_vault();
    let vault = dir.path();
    let mut d = start_daemon(vault);
    let base = d.http.clone();
    let token = read_token_file(vault)["token"]
        .as_str()
        .unwrap()
        .to_string();

    // Export: gzip bytes with auth, 401 without.
    let resp = client()
        .get(format!("http://{base}/api/export"))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let resp = client()
        .get(format!("http://{base}/api/export"))
        .bearer_auth(&token)
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains(".tar.gz"));
    let bytes = resp.bytes().unwrap();
    assert!(
        bytes.len() > 2 && bytes[0] == 0x1f && bytes[1] == 0x8b,
        "not gzip"
    );

    // Housekeeping through the bridge.
    let rebuilt = api_admin_ok(&base, &token, json!({"cmd": "rebuild"}));
    assert!(rebuilt["reindexed"].is_number());
    let grads = api_admin_ok(&base, &token, json!({"cmd": "graduate"}));
    assert!(grads.is_array());

    // `tuskd dashboard --no-open` prints the URL while the daemon is up.
    let out = run_cli(vault, &["dashboard", "--no-open"]);
    assert!(
        out.contains(&d.dashboard_url),
        "dashboard cmd output {out:?} missing {}",
        d.dashboard_url
    );

    // SIGTERM: clean shutdown removes the operator token file.
    let _ = Command::new("kill")
        .arg(d.child.id().to_string())
        .status()
        .unwrap();
    wait_exit(&mut d.child, 2);
    assert!(
        !vault.join(".tusk").join("admin-token").exists(),
        "admin-token must be removed on shutdown"
    );

    // With no daemon, `tuskd dashboard` fails with a helpful error.
    let out = bin()
        .arg("--vault")
        .arg(vault)
        .args(["dashboard", "--no-open"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "dashboard must fail without a daemon"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("daemon"),
        "stderr should mention the daemon"
    );
}

/// Overview aggregates: one round-trip serving the dashboard front page.
#[test]
fn dashboard_overview_aggregates() {
    let dir = setup_vault();
    let vault = dir.path();
    let d = start_daemon(vault);
    let base = &d.http;
    let token = read_token_file(vault)["token"]
        .as_str()
        .unwrap()
        .to_string();

    let agent = api_admin_ok(
        base,
        &token,
        json!({"cmd": "agent_create", "id": "scribe", "promote": ["org"]}),
    );
    let agent_token = agent["token"].as_str().unwrap().to_string();
    mcp_call(
        base,
        &agent_token,
        "memory_write",
        json!({"content": "overview fact one: glockenspiel"}),
    );
    mcp_call(
        base,
        &agent_token,
        "memory_write",
        json!({"content": "overview fact two: vibraphone"}),
    );
    let queued = mcp_call(
        base,
        &agent_token,
        "memory_promote",
        json!({"content": "org fact: overview pending item", "target_scope": "org"}),
    );
    assert_eq!(queued["action"], "queued", "promote to org: {queued}");

    let ov = api_admin_ok(base, &token, json!({"cmd": "overview"}));
    // Zero-filled default window: 14 buckets, all writes in the window.
    assert_eq!(ov["days"], 14, "overview: {ov}");
    let activity = ov["activity"].as_array().unwrap();
    assert_eq!(activity.len(), 14);
    let total: i64 = activity.iter().map(|b| b[1].as_i64().unwrap()).sum();
    assert_eq!(total, 2);
    assert_eq!(activity.last().unwrap()[1], 2, "writes land today");
    // Status fields ride along; review preview mirrors the queue.
    assert_eq!(ov["index"]["total"], 2);
    assert_eq!(ov["review_queue_depth"], 1);
    assert_eq!(ov["review_preview"].as_array().unwrap().len(), 1);
    assert_eq!(ov["agents"], 1);
    // Recent records, newest first, and per-author aggregates.
    let recent = ov["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 2);
    assert!(recent[0]["snippet"]
        .as_str()
        .unwrap()
        .contains("vibraphone"));
    let authors = ov["authors"].as_array().unwrap();
    assert_eq!(authors[0]["author"], "scribe");
    assert_eq!(authors[0]["records"], 2);
    assert_eq!(authors[0]["valid"], 2);

    // The window parameter is honored and clamped.
    let wide = api_admin_ok(base, &token, json!({"cmd": "overview", "days": 30}));
    assert_eq!(wide["activity"].as_array().unwrap().len(), 30);
    let clamped = api_admin_ok(base, &token, json!({"cmd": "overview", "days": 500}));
    assert_eq!(clamped["activity"].as_array().unwrap().len(), 90);
}

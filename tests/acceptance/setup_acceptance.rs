//! Acceptance tests for `tuskd agent setup` (DECISIONS D16): merge-not-clobber
//! client config editing, idempotency, removal, and per-client file shapes.
//! Every run points $HOME and the cwd at temp dirs so the host machine's real
//! client configs are never touched.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

struct World {
    _root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    vault: PathBuf,
}

fn world() -> World {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let vault = project.join("vault");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    World {
        _root: root,
        home,
        project,
        vault,
    }
}

fn run(w: &World, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tuskd"))
        .arg("--vault")
        .arg(&w.vault)
        .args(args)
        .current_dir(&w.project)
        .env("HOME", &w.home)
        .env_remove("OPENTUSK_VAULT")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn run_ok(w: &World, args: &[&str]) -> String {
    let (ok, stdout, stderr) = run(w, args);
    assert!(ok, "tuskd {args:?} failed:\n{stdout}\n{stderr}");
    stdout
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("bad JSON at {}: {e}", path.display()))
}

fn desktop_config(w: &World) -> PathBuf {
    #[cfg(target_os = "macos")]
    let rel = "Library/Application Support/Claude/claude_desktop_config.json";
    #[cfg(not(target_os = "macos"))]
    let rel = ".config/Claude/claude_desktop_config.json";
    w.home.join(rel)
}

fn backups_in(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn claude_desktop_setup_merges_backs_up_and_is_idempotent() {
    let w = world();
    // Pre-existing config with another server and an unrelated top-level key.
    let path = desktop_config(&w);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"globalShortcut": "Cmd+K", "mcpServers": {"other": {"command": "other-tool"}}}"#,
    )
    .unwrap();

    let stdout = run_ok(&w, &["agent", "setup", "claude-desktop", "--yes"]);
    assert!(stdout.contains("created agent claude-desktop"), "{stdout}");
    assert!(stdout.contains("verified: MCP handshake ok"), "{stdout}");

    let cfg = read_json(&path);
    // Merge preserved everything that was already there.
    assert_eq!(cfg["globalShortcut"], "Cmd+K");
    assert_eq!(cfg["mcpServers"]["other"]["command"], "other-tool");
    // Our entry: absolute binary, absolute vault, stdio identity.
    let entry = &cfg["mcpServers"]["opentusk"];
    assert_eq!(entry["command"], env!("CARGO_BIN_EXE_tuskd"));
    let args: Vec<&str> = entry["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        args,
        vec![
            "--vault",
            w.vault.display().to_string().as_str(),
            "mcp",
            "--agent",
            "claude-desktop"
        ]
    );
    assert_eq!(
        backups_in(path.parent().unwrap()),
        1,
        "one backup after first write"
    );

    // Second run: agent exists, config identical — nothing rewritten.
    let stdout = run_ok(&w, &["agent", "setup", "claude-desktop", "--yes"]);
    assert!(stdout.contains("already up to date"), "{stdout}");
    assert!(!stdout.contains("created agent"), "{stdout}");
    assert_eq!(
        backups_in(path.parent().unwrap()),
        1,
        "no backup when nothing changed"
    );
}

#[test]
fn remove_deletes_only_our_entry() {
    let w = world();
    let path = desktop_config(&w);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"mcpServers": {"other": {"command": "other-tool"}}}"#,
    )
    .unwrap();

    run_ok(&w, &["agent", "setup", "claude-desktop", "--yes"]);
    let stdout = run_ok(&w, &["agent", "setup", "claude-desktop", "--remove"]);
    assert!(stdout.contains("removed"), "{stdout}");

    let cfg = read_json(&path);
    assert!(cfg["mcpServers"].get("opentusk").is_none());
    assert_eq!(cfg["mcpServers"]["other"]["command"], "other-tool");

    // Removing again is a no-op, not an error.
    let stdout = run_ok(&w, &["agent", "setup", "claude-desktop", "--remove"]);
    assert!(stdout.contains("already up to date"), "{stdout}");
}

#[test]
fn project_scoped_clients_write_into_the_project() {
    let w = world();
    run_ok(&w, &["agent", "setup", "claude-code", "--yes"]);
    run_ok(&w, &["agent", "setup", "vscode", "--yes"]);

    let mcp = read_json(&w.project.join(".mcp.json"));
    assert_eq!(
        mcp["mcpServers"]["opentusk"]["command"],
        env!("CARGO_BIN_EXE_tuskd")
    );

    // VS Code uses a "servers" section and explicit type.
    let vs = read_json(&w.project.join(".vscode/mcp.json"));
    assert_eq!(vs["servers"]["opentusk"]["type"], "stdio");
    assert!(vs.get("mcpServers").is_none());
}

#[test]
fn cursor_writes_home_config() {
    let w = world();
    run_ok(&w, &["agent", "setup", "cursor", "--yes"]);
    let cfg = read_json(&w.home.join(".cursor/mcp.json"));
    let args = cfg["mcpServers"]["opentusk"]["args"].as_array().unwrap();
    assert_eq!(args.last().unwrap(), "cursor");
}

#[test]
fn codex_toml_preserves_comments_and_unrelated_keys() {
    let w = world();
    let path = w.home.join(".codex/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "# my codex config\nmodel = \"o4-mini\"\n\n[mcp_servers.other]\ncommand = \"other-tool\"\n",
    )
    .unwrap();

    run_ok(&w, &["agent", "setup", "codex", "--yes"]);
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("# my codex config"),
        "comment survived:\n{raw}"
    );
    assert!(raw.contains("model = \"o4-mini\""), "{raw}");
    assert!(raw.contains("[mcp_servers.other]"), "{raw}");
    assert!(raw.contains("[mcp_servers.opentusk]"), "{raw}");
    assert!(raw.contains("--agent"), "{raw}");

    run_ok(&w, &["agent", "setup", "codex", "--remove"]);
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("[mcp_servers.opentusk]"), "{raw}");
    assert!(raw.contains("# my codex config"), "{raw}");
    assert!(raw.contains("[mcp_servers.other]"), "{raw}");
}

#[test]
fn print_is_a_pure_dry_run() {
    let w = world();
    run_ok(&w, &["init"]);
    let stdout = run_ok(&w, &["agent", "setup", "claude-desktop", "--print"]);
    assert!(stdout.contains("opentusk"), "{stdout}");
    assert!(
        !desktop_config(&w).exists(),
        "print must not write the config"
    );
    // And it must not have created the agent either.
    let agents = run_ok(&w, &["agent", "list"]);
    assert!(!agents.contains("claude-desktop"), "{agents}");
}

#[test]
fn http_is_refused_where_unsupported_and_works_via_rotate() {
    let w = world();
    let (ok, _, stderr) = run(&w, &["agent", "setup", "claude-desktop", "--yes", "--http"]);
    assert!(!ok);
    assert!(stderr.contains("stdio"), "{stderr}");

    // HTTP for a supporting client embeds a live token.
    run_ok(&w, &["init"]);
    let stdout = run_ok(&w, &["agent", "setup", "cursor", "--yes", "--http"]);
    assert!(stdout.contains("created agent cursor"), "{stdout}");
    let cfg = read_json(&w.home.join(".cursor/mcp.json"));
    let auth = cfg["mcpServers"]["opentusk"]["headers"]["Authorization"]
        .as_str()
        .unwrap();
    assert!(auth.starts_with("Bearer tusk_"), "{auth}");

    // Re-running rotates: the embedded token changes.
    let stdout = run_ok(&w, &["agent", "setup", "cursor", "--yes", "--http"]);
    assert!(stdout.contains("rotating token"), "{stdout}");
    let cfg2 = read_json(&w.home.join(".cursor/mcp.json"));
    let auth2 = cfg2["mcpServers"]["opentusk"]["headers"]["Authorization"]
        .as_str()
        .unwrap();
    assert!(auth2.starts_with("Bearer tusk_"), "{auth2}");
    assert_ne!(auth, auth2, "rotate must mint a fresh token");
}

#[test]
fn token_rotate_prints_a_fresh_token_once() {
    let w = world();
    run_ok(&w, &["init"]);
    let created = run_ok(&w, &["agent", "create", "bot"]);
    let old = created
        .lines()
        .find_map(|l| l.trim().strip_prefix("token: "))
        .unwrap()
        .to_string();
    let rotated = run_ok(&w, &["agent", "token", "rotate", "bot"]);
    let new = rotated
        .lines()
        .find(|l| l.trim().starts_with("tusk_"))
        .unwrap()
        .trim();
    assert_ne!(old, new);
}

#[test]
fn malformed_client_config_is_refused_untouched() {
    let w = world();
    let path = desktop_config(&w);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{not json").unwrap();
    let (ok, _, stderr) = run(&w, &["agent", "setup", "claude-desktop", "--yes"]);
    assert!(!ok);
    assert!(stderr.contains("refusing to touch"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
}

#[test]
fn setup_list_shows_all_clients() {
    let w = world();
    let stdout = run_ok(&w, &["agent", "setup", "list"]);
    for name in [
        "claude-code",
        "claude-desktop",
        "cursor",
        "codex",
        "vscode",
        "print",
    ] {
        assert!(stdout.contains(name), "missing {name}:\n{stdout}");
    }
}

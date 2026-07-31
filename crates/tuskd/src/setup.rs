//! `tuskd agent setup <client>` — write a client's MCP config so it talks to
//! this vault (DECISIONS D16). Merge-not-clobber: only the `opentusk` entry
//! in the client file is ever touched, and a timestamped backup is taken
//! before any change.

use crate::admin::AdminRequest;
use crate::config;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tusk_core::error::CoreError;

/// Server key written into every client config.
const SERVER_NAME: &str = "opentusk";
/// Default grants for an agent auto-created by setup: read shared knowledge,
/// write project scopes (own `agent:<id>` scope is always writable).
const DEFAULT_READ: &str = "org,project:*";
const DEFAULT_WRITE: &str = "project:*";

pub struct SetupArgs {
    pub client: Option<String>,
    pub agent: Option<String>,
    pub http: bool,
    pub print: bool,
    pub remove: bool,
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Codex,
    Vscode,
}

const ALL_CLIENTS: [Client; 5] = [
    Client::ClaudeCode,
    Client::ClaudeDesktop,
    Client::Cursor,
    Client::Codex,
    Client::Vscode,
];

impl Client {
    fn name(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude-code",
            Client::ClaudeDesktop => "claude-desktop",
            Client::Cursor => "cursor",
            Client::Codex => "codex",
            Client::Vscode => "vscode",
        }
    }

    fn parse(s: &str) -> Option<Client> {
        ALL_CLIENTS.into_iter().find(|c| c.name() == s)
    }

    /// Where this client's MCP config lives. Project-scoped clients resolve
    /// against the current directory; the rest against $HOME.
    fn config_path(self, home: &Path) -> PathBuf {
        match self {
            Client::ClaudeCode => PathBuf::from(".mcp.json"),
            Client::Vscode => PathBuf::from(".vscode/mcp.json"),
            Client::ClaudeDesktop => crate::platform::claude_desktop_config_path(home),
            Client::Cursor => home.join(".cursor/mcp.json"),
            Client::Codex => home.join(".codex/config.toml"),
        }
    }

    /// Top-level key holding the server map in JSON configs.
    fn section(self) -> &'static str {
        match self {
            Client::Vscode => "servers",
            _ => "mcpServers",
        }
    }

    fn supports_http(self) -> bool {
        // Claude Desktop and Codex only launch stdio servers.
        !matches!(self, Client::ClaudeDesktop | Client::Codex)
    }

    fn next_step(self) -> &'static str {
        match self {
            Client::ClaudeCode => "picked up automatically — check with /mcp inside Claude Code",
            Client::ClaudeDesktop => "restart Claude Desktop to load it",
            Client::Cursor => "restart Cursor (or enable it under Settings → MCP)",
            Client::Codex => "takes effect on the next codex session",
            Client::Vscode => "reload the VS Code window to load it",
        }
    }
}

pub fn run(vault: &Path, args: SetupArgs) -> Result<(), CoreError> {
    let client_arg = args.client.as_deref().unwrap_or("list");
    if client_arg == "list" {
        return list();
    }
    if client_arg == "print" {
        return print_generic(vault, &args);
    }
    let client = Client::parse(client_arg).ok_or_else(|| {
        CoreError::Other(format!(
            "unknown client {client_arg:?} — expected one of: claude-code, claude-desktop, \
             cursor, codex, vscode, print, list"
        ))
    })?;
    let agent_id = args.agent.clone().unwrap_or_else(|| client.name().into());

    if args.http && !client.supports_http() {
        return Err(CoreError::Other(format!(
            "{} only supports stdio MCP servers — drop --http",
            client.name()
        )));
    }

    let home = home_dir()?;
    let path = client.config_path(&home);

    if args.remove {
        let outcome = match client {
            Client::Codex => edit_toml_file(&path, None)?,
            _ => edit_json_file(&path, client.section(), None)?,
        };
        report_outcome(&path, &outcome, "removed");
        return Ok(());
    }

    // Everything below needs a vault (port, agent identity).
    ensure_vault(vault, &args)?;
    let cfg = config::load(vault)?;
    let abs_vault = absolute(vault)?;

    // --print is a pure dry run: no agent creation, no token rotation.
    let token = if args.print {
        args.http
            .then(|| "<TOKEN — run: tuskd agent token rotate>".into())
    } else {
        let created = ensure_agent(vault, &cfg, &agent_id)?;
        match (args.http, created) {
            (false, _) => None,
            (true, Some(token)) => Some(token),
            (true, None) => {
                println!("rotating token for existing agent {agent_id} (the old token dies now)");
                let data = crate::commands::admin_route(
                    vault,
                    &AdminRequest::AgentTokenRotate {
                        id: agent_id.clone(),
                    },
                )?;
                Some(data["token"].as_str().unwrap_or_default().to_string())
            }
        }
    };

    let entry = entry_for(
        client,
        &abs_vault,
        &agent_id,
        cfg.http_port,
        token.as_deref(),
    )?;

    if args.print {
        println!(
            "{} would get this {SERVER_NAME} entry under {:?}:",
            path.display(),
            client.section()
        );
        println!("{}", serde_json::to_string_pretty(&entry)?);
        return Ok(());
    }

    let outcome = match client {
        Client::Codex => edit_toml_file(&path, Some(&entry))?,
        _ => edit_json_file(&path, client.section(), Some(entry.clone()))?,
    };
    report_outcome(&path, &outcome, "configured");

    verify_handshake(vault, &agent_id)?;
    println!("verified: MCP handshake ok (agent {agent_id})");
    println!("next: {}", client.next_step());
    Ok(())
}

/// `tuskd agent token rotate` lives here so the CLI wording matches setup's.
pub fn rotate_token(vault: &Path, id: &str) -> Result<(), CoreError> {
    let data =
        crate::commands::admin_route(vault, &AdminRequest::AgentTokenRotate { id: id.into() })?;
    println!("new token for {id} (shown ONCE — the old token is dead):");
    println!("{}", data["token"].as_str().unwrap_or_default());
    Ok(())
}

fn list() -> Result<(), CoreError> {
    let home = home_dir()?;
    println!("supported clients (tuskd agent setup <client>):");
    for client in ALL_CLIENTS {
        let path = client.config_path(&home);
        let status = match read_entry_present(client, &path) {
            Some(true) => "configured",
            Some(false) => "not configured",
            None => "no config file",
        };
        println!("  {:<15} {:<15} {}", client.name(), status, path.display());
    }
    println!(
        "  {:<15} {:<15} prints a generic snippet for other clients",
        "print", "-"
    );
    Ok(())
}

fn print_generic(vault: &Path, args: &SetupArgs) -> Result<(), CoreError> {
    let agent_id = args.agent.clone().unwrap_or_else(|| "my-agent".into());
    let abs_vault = absolute(vault)?;
    let exe = tuskd_exe();
    let port = config::load(vault)
        .map(|c| c.http_port)
        .unwrap_or(config::DEFAULT_HTTP_PORT);
    println!("stdio (recommended — no token needed):");
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "command": exe,
            "args": ["--vault", abs_vault, "mcp", "--agent", agent_id],
        }))?
    );
    println!();
    println!("streamable HTTP (mint a token with: tuskd agent token rotate {agent_id}):");
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "url": format!("http://127.0.0.1:{port}/mcp"),
            "headers": {"Authorization": "Bearer <TOKEN>"},
        }))?
    );
    Ok(())
}

fn ensure_vault(vault: &Path, args: &SetupArgs) -> Result<(), CoreError> {
    if vault.join(".tusk").is_dir() {
        return Ok(());
    }
    if !args.yes {
        return Err(CoreError::Other(format!(
            "no vault at {} — run `tuskd init` first, or re-run with --yes to auto-init",
            vault.display()
        )));
    }
    crate::commands::init(vault)
}

/// Create the agent if it doesn't exist. Returns the one-time token when a
/// new agent was created; None when it already existed.
fn ensure_agent(vault: &Path, cfg: &config::Config, id: &str) -> Result<Option<String>, CoreError> {
    let agents = crate::commands::admin_route(vault, &AdminRequest::AgentList)?;
    let existing = agents
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["id"].as_str() == Some(id));
    if let Some(agent) = existing {
        if agent["revoked"].as_bool() == Some(true) {
            return Err(CoreError::Denied(format!(
                "agent {id} is revoked — pick another id with --agent"
            )));
        }
        return Ok(None);
    }
    let split = |s: &str| s.split(',').map(str::to_string).collect::<Vec<_>>();
    let data = crate::commands::admin_route(
        vault,
        &AdminRequest::AgentCreate {
            id: id.to_string(),
            read: split(DEFAULT_READ),
            write: split(DEFAULT_WRITE),
            promote: Vec::new(),
            show_key: false,
        },
    )?;
    println!("created agent {id} (read: {DEFAULT_READ}; write: {DEFAULT_WRITE})");
    crate::commands::print_created_agent(cfg, id, &data);
    Ok(data["token"].as_str().map(str::to_string))
}

/// The config entry for one client, stdio or HTTP.
fn entry_for(
    client: Client,
    abs_vault: &str,
    agent_id: &str,
    port: u16,
    token: Option<&str>,
) -> Result<Value, CoreError> {
    let entry = match token {
        Some(token) => {
            let mut e = json!({
                "url": format!("http://127.0.0.1:{port}/mcp"),
                "headers": {"Authorization": format!("Bearer {token}")},
            });
            if client == Client::Vscode {
                e["type"] = json!("http");
            }
            e
        }
        None => {
            let mut e = json!({
                "command": tuskd_exe(),
                "args": ["--vault", abs_vault, "mcp", "--agent", agent_id],
            });
            if client == Client::Vscode {
                e["type"] = json!("stdio");
            }
            e
        }
    };
    Ok(entry)
}

struct Outcome {
    changed: bool,
    backup: Option<PathBuf>,
}

fn report_outcome(path: &Path, outcome: &Outcome, verb: &str) {
    if !outcome.changed {
        println!("{} — already up to date, nothing written", path.display());
        return;
    }
    match &outcome.backup {
        Some(bak) => println!("{verb} {} (backup: {})", path.display(), bak.display()),
        None => println!("{verb} {}", path.display()),
    }
}

/// Does the client's config already carry an `opentusk` entry?
/// None = no file, Some(bool) = file exists / entry present.
/// A client as the wizard sees it (D36).
pub(crate) struct ClientStatus {
    pub(crate) name: &'static str,
    /// Present on this machine, by cheap heuristic (binary on PATH or
    /// the client's config dir existing).
    pub(crate) installed: bool,
    /// Its config already has the opentusk entry.
    pub(crate) configured: bool,
}

/// Detect installed AI clients for the wizard's offers. Heuristics only —
/// a miss is fine (the wizard points at `tuskd agent setup list`).
pub(crate) fn detect_clients() -> Vec<ClientStatus> {
    let Ok(home) = home_dir() else {
        return Vec::new();
    };
    ALL_CLIENTS
        .into_iter()
        .map(|client| {
            let path = client.config_path(&home);
            let installed = match client {
                Client::ClaudeCode => binary_on_path("claude"),
                Client::ClaudeDesktop => path.parent().is_some_and(Path::exists),
                Client::Cursor => home.join(".cursor").is_dir(),
                Client::Codex => home.join(".codex").is_dir(),
                Client::Vscode => Path::new(".vscode").is_dir(),
            };
            ClientStatus {
                name: client.name(),
                installed,
                configured: read_entry_present(client, &path) == Some(true),
            }
        })
        .collect()
}

fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

fn read_entry_present(client: Client, path: &Path) -> Option<bool> {
    let raw = std::fs::read_to_string(path).ok()?;
    Some(match client {
        Client::Codex => raw
            .parse::<toml_edit::DocumentMut>()
            .ok()
            .and_then(|d| {
                d.get("mcp_servers")
                    .and_then(|s| s.get(SERVER_NAME))
                    .map(|_| true)
            })
            .unwrap_or(false),
        _ => serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| {
                v.get(client.section())
                    .and_then(|s| s.get(SERVER_NAME))
                    .map(|_| true)
            })
            .unwrap_or(false),
    })
}

/// Insert/replace (`Some(entry)`) or remove (`None`) `section.opentusk` in a
/// JSON config, preserving everything else in the file.
fn edit_json_file(path: &Path, section: &str, entry: Option<Value>) -> Result<Outcome, CoreError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(CoreError::io(path.display().to_string(), e)),
    };
    let mut root: Value = match &existing {
        Some(raw) if !raw.trim().is_empty() => serde_json::from_str(raw).map_err(|e| {
            CoreError::Other(format!(
                "{} is not valid JSON ({e}) — refusing to touch it; fix or move it first",
                path.display()
            ))
        })?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err(CoreError::Other(format!(
            "{} is not a JSON object — refusing to touch it",
            path.display()
        )));
    }

    match entry {
        Some(entry) => {
            let map = root.as_object_mut().unwrap_or_else(|| unreachable!());
            let sect = map.entry(section.to_string()).or_insert_with(|| json!({}));
            if !sect.is_object() {
                return Err(CoreError::Other(format!(
                    "{} has a non-object {section:?} key — refusing to touch it",
                    path.display()
                )));
            }
            sect[SERVER_NAME] = entry;
        }
        None => {
            let present = root
                .get_mut(section)
                .and_then(|s| s.as_object_mut())
                .map(|s| s.remove(SERVER_NAME).is_some())
                .unwrap_or(false);
            if !present {
                return Ok(Outcome {
                    changed: false,
                    backup: None,
                });
            }
        }
    }

    let rendered = format!("{}\n", serde_json::to_string_pretty(&root)?);
    write_with_backup(path, existing.as_deref(), &rendered)
}

/// Same as `edit_json_file`, for Codex's `config.toml` — via toml_edit so the
/// rest of the user's file (comments, formatting) survives untouched.
fn edit_toml_file(path: &Path, entry: Option<&Value>) -> Result<Outcome, CoreError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(CoreError::io(path.display().to_string(), e)),
    };
    let mut doc: toml_edit::DocumentMut =
        existing.as_deref().unwrap_or("").parse().map_err(|e| {
            CoreError::Other(format!(
                "{} is not valid TOML ({e}) — refusing to touch it; fix or move it first",
                path.display()
            ))
        })?;

    match entry {
        Some(entry) => {
            let mut table = toml_edit::Table::new();
            table["command"] = toml_edit::value(entry["command"].as_str().unwrap_or("tuskd"));
            let mut args = toml_edit::Array::new();
            for a in entry["args"].as_array().into_iter().flatten() {
                args.push(a.as_str().unwrap_or_default());
            }
            table["args"] = toml_edit::value(args);
            let servers = doc
                .entry("mcp_servers")
                .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
            let servers = servers.as_table_mut().ok_or_else(|| {
                CoreError::Other(format!(
                    "{} has a non-table mcp_servers key — refusing to touch it",
                    path.display()
                ))
            })?;
            servers.set_implicit(true);
            servers.insert(SERVER_NAME, toml_edit::Item::Table(table));
        }
        None => {
            let present = doc
                .get_mut("mcp_servers")
                .and_then(|s| s.as_table_mut())
                .map(|s| s.remove(SERVER_NAME).is_some())
                .unwrap_or(false);
            if !present {
                return Ok(Outcome {
                    changed: false,
                    backup: None,
                });
            }
        }
    }

    write_with_backup(path, existing.as_deref(), &doc.to_string())
}

/// No-op if content is unchanged; otherwise back the old file up beside
/// itself (timestamped, never overwriting a prior backup) and write.
fn write_with_backup(
    path: &Path,
    existing: Option<&str>,
    rendered: &str,
) -> Result<Outcome, CoreError> {
    if existing == Some(rendered) {
        return Ok(Outcome {
            changed: false,
            backup: None,
        });
    }
    let backup = match existing {
        Some(old) if !old.trim().is_empty() => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut bak = path.with_file_name(format!(
                "{}.bak-{stamp}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("config")
            ));
            let mut n = 1;
            while bak.exists() {
                bak = path.with_file_name(format!(
                    "{}.bak-{stamp}-{n}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("config")
                ));
                n += 1;
            }
            std::fs::write(&bak, old).map_err(|e| CoreError::io(bak.display().to_string(), e))?;
            Some(bak)
        }
        _ => None,
    };
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        }
    }
    std::fs::write(path, rendered).map_err(|e| CoreError::io(path.display().to_string(), e))?;
    Ok(Outcome {
        changed: true,
        backup,
    })
}

/// Prove the written config actually works: spawn `tuskd mcp --agent <id>`
/// exactly as the client will, drive one MCP initialize, and tear it down.
fn verify_handshake(vault: &Path, agent_id: &str) -> Result<(), CoreError> {
    let mut child = Command::new(tuskd_exe())
        .arg("--vault")
        .arg(vault)
        .args(["mcp", "--agent", agent_id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| CoreError::Other(format!("spawn tuskd mcp: {e}")))?;

    let result = (|| -> Result<(), CoreError> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Other("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Other("no stdout".into()))?;
        writeln!(
            stdin,
            "{}",
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "tuskd-setup", "version": env!("CARGO_PKG_VERSION")},
                }
            })
        )
        .map_err(|e| CoreError::Other(format!("handshake write: {e}")))?;
        drop(stdin); // stdin EOF also tells the server to exit after replying

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let line = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| CoreError::Other("MCP handshake timed out after 10s".into()))?;
        let reply: Value = serde_json::from_str(line.trim())
            .map_err(|e| CoreError::Other(format!("bad handshake reply ({e}): {line}")))?;
        if reply.get("result").is_none() {
            return Err(CoreError::Other(format!("handshake failed: {line}")));
        }
        Ok(())
    })();

    // Never leave a child behind, success or failure.
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn tuskd_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "tuskd".into())
}

fn absolute(path: &Path) -> Result<String, CoreError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| CoreError::Other(format!("cwd: {e}")))?
            .join(path)
    };
    Ok(abs.display().to_string())
}

fn home_dir() -> Result<PathBuf, CoreError> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CoreError::Other("$HOME is not set".into()))
}

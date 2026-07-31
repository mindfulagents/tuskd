//! CLI command dispatch. One-shot commands route through a live daemon's UDS
//! admin endpoint, or run embedded under the vault lock (DECISIONS D6).

use crate::admin::{AdminRequest, AdminResponse};
use crate::cli::{
    AgentCommand, Cli, Command, IndexCommand, KeyCommand, ReviewCommand, TokenCommand,
};
use crate::config;
use crate::runtime::CoreHost;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use tusk_core::error::CoreError;

pub fn run(cli: Cli) -> i32 {
    let vault = config::resolve_vault(cli.vault.clone());
    match dispatch(&vault, cli.command) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn dispatch(vault: &std::path::Path, command: Command) -> Result<(), CoreError> {
    match command {
        Command::Init => init(vault),
        Command::Start { detach } => {
            if detach {
                start_detached(vault)
            } else {
                let cfg = config::load(vault)?;
                crate::daemon::run(cfg)
            }
        }
        Command::Stop => stop_daemon(vault, false),
        Command::Restart { detach } => {
            stop_daemon(vault, true)?;
            if detach {
                start_detached(vault)
            } else {
                let cfg = config::load(vault)?;
                crate::daemon::run(cfg)
            }
        }
        Command::Mcp { agent } => {
            let cfg = config::load(vault)?;
            crate::stdio::run(cfg, agent)
        }
        Command::Status => {
            let data = admin_route(vault, &AdminRequest::Status)?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            Ok(())
        }
        Command::Search {
            query,
            scope,
            as_of,
            k,
        } => {
            let data = admin_route(
                vault,
                &AdminRequest::Search {
                    query,
                    scopes: scope,
                    as_of,
                    k: Some(k),
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            Ok(())
        }
        Command::Index { command } => match command {
            Some(IndexCommand::Rebuild) => {
                let data = admin_route(vault, &AdminRequest::Rebuild)?;
                println!(
                    "reindexed {} records",
                    data.get("reindexed").and_then(|v| v.as_i64()).unwrap_or(0)
                );
                Ok(())
            }
            None => {
                let data = admin_route(vault, &AdminRequest::Status)?;
                println!("{}", serde_json::to_string_pretty(&data)?);
                Ok(())
            }
        },
        Command::Review { command } => match command {
            ReviewCommand::List => {
                let data = admin_route(vault, &AdminRequest::ReviewList)?;
                let items = data.as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("review queue is empty");
                } else {
                    for item in items {
                        println!(
                            "{} {} {} by {} at {}",
                            item["qid"].as_str().unwrap_or("?"),
                            item["candidate"]["type"].as_str().unwrap_or("?"),
                            item["candidate"]["scope"].as_str().unwrap_or("?"),
                            item["author"].as_str().unwrap_or("?"),
                            item["submitted_at"].as_str().unwrap_or("?"),
                        );
                    }
                }
                Ok(())
            }
            ReviewCommand::Approve { qid } => {
                let data = admin_route(vault, &AdminRequest::Review { qid, approve: true })?;
                println!("{}", serde_json::to_string_pretty(&data)?);
                Ok(())
            }
            ReviewCommand::Reject { qid } => {
                let data = admin_route(
                    vault,
                    &AdminRequest::Review {
                        qid,
                        approve: false,
                    },
                )?;
                println!("{}", serde_json::to_string_pretty(&data)?);
                Ok(())
            }
        },
        Command::Graduate => {
            let data = admin_route(vault, &AdminRequest::Graduate)?;
            let n = data.as_array().map(|a| a.len()).unwrap_or(0);
            if n == 0 {
                println!("no candidates met the graduation thresholds");
            } else {
                println!("{}", serde_json::to_string_pretty(&data)?);
            }
            Ok(())
        }
        Command::Sync { command } => sync_command(vault, command),
        Command::Dashboard { no_open } => dashboard(vault, no_open),
        Command::Agent { command } => agent_command(vault, command),
        Command::Export { archive } => {
            let count = crate::archive::export(vault, &archive)?;
            println!("exported {count} files to {}", archive.display());
            Ok(())
        }
        Command::Import { archive } => crate::archive::import(vault, &archive),
    }
}

pub(crate) fn init(vault: &std::path::Path) -> Result<(), CoreError> {
    std::fs::create_dir_all(vault).map_err(|e| CoreError::io(vault.display().to_string(), e))?;
    for sub in ["memory", "skills", ".tusk"] {
        let dir = vault.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
    }
    let cfg_path = config::config_path(vault);
    if !cfg_path.exists() {
        std::fs::write(&cfg_path, config::DEFAULT_TOML)
            .map_err(|e| CoreError::io(cfg_path.display().to_string(), e))?;
    }
    // Prove the core opens cleanly (also creates the index + FTS5 probe).
    let cfg = config::load(vault)?;
    let host = CoreHost::open(&cfg, false)?;
    host.shutdown();
    println!("initialized vault at {}", vault.display());
    println!("next: tuskd agent create <id>   # then: tuskd start");
    Ok(())
}

/// `tuskd start --detach` (D18): spawn the daemon in its own process group
/// with output in `.tusk/daemon.log`, then wait until its socket answers.
fn start_detached(vault: &std::path::Path) -> Result<(), CoreError> {
    let cfg = config::load(vault)?;
    if UnixStream::connect(&cfg.uds_path).is_ok() {
        return Err(CoreError::Other(
            "a daemon is already running for this vault".into(),
        ));
    }
    let abs_vault = std::fs::canonicalize(vault).unwrap_or_else(|_| vault.to_path_buf());
    let log = vault.join(".tusk").join("daemon.log");
    let exe = std::env::current_exe().map_err(|e| CoreError::Other(format!("current_exe: {e}")))?;
    let pid = crate::platform::spawn_detached(&exe, &abs_vault, &log)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while UnixStream::connect(&cfg.uds_path).is_err() {
        if std::time::Instant::now() > deadline {
            return Err(CoreError::Other(format!(
                "daemon (pid {pid}) did not come up within 10s — see {}",
                log.display()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!("daemon started (pid {pid}); log: {}", log.display());
    println!("stop: tuskd stop   status: tuskd status   ui: tuskd dashboard");
    Ok(())
}

/// `tuskd stop` (D18): graceful shutdown over the UDS admin plane, then wait
/// for the vault lock to release so the vault is immediately reusable.
fn stop_daemon(vault: &std::path::Path, tolerate_absent: bool) -> Result<(), CoreError> {
    let cfg = config::load(vault)?;
    if UnixStream::connect(&cfg.uds_path).is_err() {
        if tolerate_absent {
            println!("no daemon was running");
            return Ok(());
        }
        return Err(CoreError::Other(
            "no daemon is running for this vault".into(),
        ));
    }
    match admin_route(vault, &AdminRequest::Shutdown) {
        Ok(_) => {}
        // Pre-D18 daemons don't know the shutdown verb; their admin parser
        // rejects it by name. Fall back to SIGTERM via the pid recorded in
        // .tusk/lock — every shipped version shuts down cleanly on SIGTERM.
        Err(e) if e.to_string().contains("unknown variant `shutdown`") => {
            let lock_path = vault.join(".tusk").join("lock");
            let raw = std::fs::read_to_string(&lock_path)
                .map_err(|e| CoreError::io(lock_path.display().to_string(), e))?;
            let pid: u32 = raw
                .split_whitespace()
                .next()
                .unwrap_or("")
                .parse()
                .map_err(|_| {
                    CoreError::Other(format!(
                        "no pid in {} — kill the daemon manually",
                        lock_path.display()
                    ))
                })?;
            println!("daemon predates the shutdown verb — sending SIGTERM to pid {pid}");
            crate::platform::terminate_pid(pid)?;
        }
        Err(e) => return Err(e),
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match crate::platform::VaultLock::acquire(vault) {
            Ok(lock) => {
                drop(lock);
                break;
            }
            Err(_) if std::time::Instant::now() <= deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => {
                return Err(CoreError::Other(
                    "daemon acknowledged the stop but did not exit within 10s".into(),
                ));
            }
        }
    }
    println!("daemon stopped");
    Ok(())
}

/// `tuskd dashboard`: resolve the running daemon's dashboard URL from
/// `.tusk/admin-token` (liveness-checked via the UDS socket).
fn dashboard(vault: &std::path::Path, no_open: bool) -> Result<(), CoreError> {
    let cfg = config::load(vault)?;
    if UnixStream::connect(&cfg.uds_path).is_err() {
        return Err(CoreError::Other(
            "no daemon is running for this vault — start one with `tuskd start`".into(),
        ));
    }
    let path = crate::dashboard::token_file_path(vault);
    let raw =
        std::fs::read_to_string(&path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
    let info: Value = serde_json::from_str(&raw)?;
    let url = info
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Other("admin-token file has no url".into()))?;
    println!("dashboard: {url}");
    if !no_open {
        crate::platform::open_url(url);
    }
    Ok(())
}

fn agent_command(vault: &std::path::Path, command: AgentCommand) -> Result<(), CoreError> {
    match command {
        AgentCommand::Create {
            id,
            read,
            write,
            promote,
            show_key,
        } => {
            let cfg = config::load(vault)?;
            let data = admin_route(
                vault,
                &AdminRequest::AgentCreate {
                    id: id.clone(),
                    read,
                    write,
                    promote,
                    show_key,
                },
            )?;
            print_created_agent(&cfg, &id, &data);
            Ok(())
        }
        AgentCommand::Grant { id, verb, scope } => {
            admin_route(vault, &AdminRequest::AgentGrant { id, verb, scope })?;
            println!("ok");
            Ok(())
        }
        AgentCommand::Revoke { id } => {
            admin_route(vault, &AdminRequest::AgentRevoke { id })?;
            println!("ok");
            Ok(())
        }
        AgentCommand::List => {
            let data = admin_route(vault, &AdminRequest::AgentList)?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            Ok(())
        }
        AgentCommand::Setup {
            client,
            agent,
            http,
            print,
            remove,
            yes,
        } => crate::setup::run(
            vault,
            crate::setup::SetupArgs {
                client,
                agent,
                http,
                print,
                remove,
                yes,
            },
        ),
        AgentCommand::Token { command } => match command {
            TokenCommand::Rotate { id } => crate::setup::rotate_token(vault, &id),
        },
        AgentCommand::Key { command } => match command {
            KeyCommand::Path { id } => {
                let path = crate::admin::agent_key_path(vault, &id);
                if path.is_file() {
                    println!("{}", path.display());
                    Ok(())
                } else {
                    Err(CoreError::NotFound(format!(
                        "no stored key for {id} — it was created with --show-key \
                         (client-side custody) or predates key storage (D17)"
                    )))
                }
            }
        },
    }
}

/// One-time credentials block shared by `agent create` and `agent setup`.
pub(crate) fn print_created_agent(cfg: &config::Config, id: &str, data: &Value) {
    let token = data["token"].as_str().unwrap_or("");
    println!("agent created: {id}");
    println!();
    println!("token: {token}");
    println!();
    match data["private_key_path"].as_str() {
        Some(path) => {
            println!("private signing key stored at: {path}");
            println!("  (0600, excluded from export; reserved for signed auth / SEAL / Walrus)");
        }
        None => {
            println!("private key (shown once, not stored):");
            println!("{}", data["private_key_pem"].as_str().unwrap_or(""));
        }
    }
    println!("MCP config (stdio):");
    println!("  {{\"command\": \"tuskd\", \"args\": [\"mcp\", \"--agent\", \"{id}\"]}}");
    println!("MCP config (streamable HTTP):");
    println!(
        "  {{\"url\": \"http://127.0.0.1:{}/mcp\", \"headers\": {{\"Authorization\": \"Bearer {token}\"}}}}",
        cfg.http_port
    );
    println!();
    println!("this token is shown ONCE — store it now.");
}

/// Send to the daemon over UDS if one is alive, else run embedded (D6).
pub(crate) fn admin_route(vault: &std::path::Path, req: &AdminRequest) -> Result<Value, CoreError> {
    let cfg = config::load(vault)?;
    if let Ok(stream) = UnixStream::connect(&cfg.uds_path) {
        return admin_over_uds(stream, req);
    }
    let host = CoreHost::open(&cfg, false)?;
    let resp = crate::admin::execute(&host.ctx, &cfg.graduation, req, false);
    host.shutdown();
    resp.into_result()
}

fn admin_over_uds(stream: UnixStream, req: &AdminRequest) -> Result<Value, CoreError> {
    let mut writer = stream
        .try_clone()
        .map_err(|e| CoreError::Other(format!("uds clone: {e}")))?;
    let header = serde_json::json!({"tusk_admin": req});
    writeln!(writer, "{header}").map_err(|e| CoreError::Other(format!("uds write: {e}")))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| CoreError::Other(format!("uds read: {e}")))?;
    let resp: AdminResponse = serde_json::from_str(line.trim())
        .map_err(|e| CoreError::Other(format!("bad admin response: {e}")))?;
    resp.into_result()
}

fn sync_command(
    vault: &std::path::Path,
    command: crate::cli::SyncCommand,
) -> Result<(), CoreError> {
    use crate::cli::SyncCommand;
    match command {
        SyncCommand::Login { url, email, code } => {
            crate::sync_cloud::login(vault, &url, &email, code)
        }
        SyncCommand::Init { repo_name, name } => crate::sync_cloud::init(vault, repo_name, name),
        SyncCommand::Repos => crate::sync_cloud::repos(vault),
        SyncCommand::Connect {
            url,
            repo,
            name,
            phrase,
        } => crate::sync_cloud::connect(vault, &url, &repo, name, phrase),
        SyncCommand::Bootstrap {
            url,
            admin_token,
            email,
            repo_name,
            name,
        } => crate::sync_cloud::bootstrap(vault, &url, &admin_token, &email, &repo_name, name),
        SyncCommand::Status => crate::sync_cloud::status(vault),
        SyncCommand::Devices => crate::sync_cloud::devices(vault),
        SyncCommand::Approve {
            device_id,
            fingerprint,
        } => crate::sync_cloud::approve(vault, &device_id, &fingerprint),
        SyncCommand::Revoke { device_id } => crate::sync_cloud::revoke(vault, &device_id),
        SyncCommand::Push => crate::sync_cloud::push(vault),
        SyncCommand::Pull => crate::sync_cloud::pull(vault),
    }
}

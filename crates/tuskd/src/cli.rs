use clap::{Parser, Subcommand};

/// tuskd — local, single-binary memory system for AI agent swarms.
#[derive(Debug, Parser)]
#[command(name = "tuskd", version, about, styles = crate::style::HELP_STYLES)]
pub struct Cli {
    /// Vault directory (defaults to $OPENTUSK_VAULT or ./vault)
    #[arg(long, global = true)]
    pub vault: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a new vault in the current directory
    Init,
    /// Guided setup: vault, daemon, AI clients, cloud sync (interactive)
    Setup,
    /// Start the daemon (owns index + watcher; serves MCP-HTTP, /status, UDS)
    Start {
        /// Run in the background, logging to .tusk/daemon.log
        #[arg(long, short = 'd')]
        detach: bool,
    },
    /// Stop the running daemon gracefully
    Stop,
    /// Stop the daemon (if running), then start it again
    Restart {
        /// Start detached, logging to .tusk/daemon.log
        #[arg(long, short = 'd')]
        detach: bool,
    },
    /// Show daemon / vault status
    Status {
        /// Machine-readable output (also the default when piped)
        #[arg(long)]
        json: bool,
    },
    /// Run an MCP stdio session (proxy to daemon, or embedded if none)
    Mcp {
        /// Agent identity for this session
        #[arg(long)]
        agent: String,
    },
    /// Manage agent identities and grants
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Index operations
    Index {
        #[command(subcommand)]
        command: Option<IndexCommand>,
    },
    /// Search the vault
    Search {
        /// Query text
        query: String,
        /// Restrict to scopes (comma-separated)
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
        /// Point-in-time query (RFC3339)
        #[arg(long)]
        as_of: Option<String>,
        /// Max results
        #[arg(long, default_value_t = 10)]
        k: usize,
    },
    /// Review queue operations
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Run the graduation scanner once
    Graduate,
    /// Cloud sync (M1): connect, approve devices, push/pull the encrypted vault
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
    /// Print (and open) the web dashboard URL for the running daemon
    Dashboard {
        /// Print the URL only; don't open a browser
        #[arg(long)]
        no_open: bool,
    },
    /// Export the vault to a tar.gz archive
    Export {
        /// Output archive path
        archive: std::path::PathBuf,
    },
    /// Import a vault archive
    Import {
        /// Input archive path
        archive: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Create an agent (prints token + MCP configs ONCE; stores the signing key)
    Create {
        id: String,
        #[arg(long, value_delimiter = ',')]
        read: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        write: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        promote: Vec<String>,
        /// Print the private key ONCE instead of storing it in the vault
        /// (client-side custody)
        #[arg(long)]
        show_key: bool,
    },
    /// Add a grant to an agent
    Grant {
        id: String,
        /// read | write | promote
        verb: String,
        scope: String,
    },
    /// Revoke an agent
    Revoke { id: String },
    /// List agents
    List,
    /// Configure an AI client (Claude Code, Claude Desktop, …) to use this vault
    Setup {
        /// claude-code | claude-desktop | cursor | codex | vscode | print | list
        client: Option<String>,
        /// Agent identity to configure (default: the client name)
        #[arg(long)]
        agent: Option<String>,
        /// Emit a streamable-HTTP config instead of stdio (rotates the token)
        #[arg(long)]
        http: bool,
        /// Print the config that would be written, without touching any file
        #[arg(long)]
        print: bool,
        /// Remove the opentusk entry from the client's config
        #[arg(long)]
        remove: bool,
        /// Non-interactive: auto-init the vault if it doesn't exist yet
        #[arg(long)]
        yes: bool,
    },
    /// Token operations
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Signing-key operations
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Print the path of an agent's stored private signing key
    Path { id: String },
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Mint a replacement bearer token (prints it ONCE; the old token dies)
    Rotate { id: String },
}

#[derive(Debug, Subcommand)]
pub enum IndexCommand {
    /// Wipe and re-walk the vault (idempotent)
    Rebuild,
}

#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// List pending review-queue items
    List,
    /// Approve a queued item
    Approve { qid: String },
    /// Reject a queued item
    Reject { qid: String },
}

#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    /// Sign in to your OpenTusk account (a code is emailed to you)
    Login {
        /// tusk-cloud base URL, e.g. https://cloud.opentusk.ai
        #[arg(long, default_value = "https://cloud.opentusk.ai")]
        url: String,
        /// Account email address
        #[arg(long)]
        email: String,
        /// The emailed code (omit to be prompted after the email is sent)
        #[arg(long)]
        code: Option<String>,
    },
    /// Create a cloud repo for this vault (requires `sync login` first)
    Init {
        /// Repo name (defaults to the vault directory's name)
        #[arg(long)]
        repo_name: Option<String>,
        /// Device display name (defaults to hostname)
        #[arg(long)]
        name: Option<String>,
    },
    /// List your account's cloud repos (requires `sync login` first)
    Repos,
    /// Rename a cloud repo (display name only; the repo id never changes)
    Rename {
        /// The new name
        name: String,
        /// Repo id (defaults to the repo this vault is connected to)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Permanently delete a cloud repo you own (the local vault is untouched)
    DeleteRepo {
        repo_id: String,
        /// Confirm: deletion is permanent and frees the plan's repo slot
        #[arg(long)]
        yes: bool,
    },
    /// Enroll this vault as a new device of an existing cloud repo
    Connect {
        /// tusk-cloud base URL (defaults to your login session's server,
        /// else https://cloud.opentusk.ai)
        #[arg(long)]
        url: Option<String>,
        /// Repo id to join
        #[arg(long)]
        repo: String,
        /// Device display name (defaults to hostname)
        #[arg(long)]
        name: Option<String>,
        /// 24-word recovery phrase (skips waiting for a device approval)
        #[arg(long)]
        phrase: Option<String>,
    },
    /// Show connection, fingerprint, and approval status
    Status,
    /// List this repo's devices with fingerprints
    Devices,
    /// Approve a pending device (fingerprint must match its screen)
    Approve {
        device_id: String,
        #[arg(long)]
        fingerprint: String,
    },
    /// Revoke a device and rotate the repo key (new phrase printed once)
    Revoke { device_id: String },
    /// Encrypt and upload the vault snapshot
    Push,
    /// Download and materialize the vault snapshot
    Pull,
}

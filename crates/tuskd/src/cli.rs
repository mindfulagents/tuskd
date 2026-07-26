use clap::{Parser, Subcommand};

/// tuskd — local, single-binary memory system for AI agent swarms.
#[derive(Debug, Parser)]
#[command(name = "tuskd", version, about)]
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
    /// Start the daemon (owns index + watcher; serves MCP-HTTP, /status, UDS)
    Start,
    /// Show daemon / vault status
    Status,
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
    /// Create an agent (prints token + MCP configs ONCE)
    Create {
        id: String,
        #[arg(long, value_delimiter = ',')]
        read: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        write: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        promote: Vec<String>,
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

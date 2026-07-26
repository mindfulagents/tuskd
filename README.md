# tuskd — the OpenTusk daemon

A local, single-binary memory system for AI agent swarms. One install gives a
machine complete agent memory: a human-readable Markdown vault, hybrid local
search (SQLite FTS5 + telemetry ranking), an MCP-native tool surface, per-agent
keys and grants, and an intelligence loop that turns raw experience into
shared, validated, versioned knowledge — with proven procedures graduating
into agent-loadable SKILL.md files.

- **Vault** — one Markdown file per memory record; files are the source of
  truth, everything else is derived and rebuildable.
- **Single-owner daemon** — exactly one `tuskd` process owns the SQLite index
  and file-watcher; stdio sessions proxy to it over a Unix socket, or run the
  core embedded when no daemon is running.
- **Identity & ACL** — every agent has an ed25519 keypair, a bearer token, and
  explicit `read`/`write`/`promote` grants per scope; every tool call passes
  one ACL choke-point.
- **The gate** — dedup → contradiction probe → per-scope policy → commit or
  review queue; corrections supersede bitemporally (nothing is edited in
  place, and `as_of` queries answer "what did we believe then?").

## Install

```sh
curl -fsSL https://get.opentusk.ai | sh
```

Detects the platform, verifies the SHA-256 checksum, installs to
`~/.local/bin` (override with `TUSKD_INSTALL_DIR`), never sudo. Pin a
release with `TUSKD_VERSION=v0.1.0`. Docs live at
[opentusk.ai](https://opentusk.ai); the page source is `site/index.html`.

## Quickstart

```sh
# (or build from source: cargo build --release; Rust stable, Apple Silicon first)
export PATH="$HOME/.local/bin:$PATH"

# 1. Initialize a vault
mkdir demo-vault && cd demo-vault
tuskd init

# 2. Create an agent — the token and MCP configs are printed exactly once
tuskd agent create hermes-dev \
  --read project:opentusk,user \
  --write project:opentusk \
  --promote project:opentusk

# 3. Start the daemon (owns index + watcher; serves MCP over HTTP :7477 + UDS)
tuskd start &
sleep 1
curl -s http://127.0.0.1:7477/status

# 4. Wire up your AI client in one command — writes the client's MCP config
#    (merge-not-clobber, with a backup), creates the agent if needed, and
#    verifies the handshake. Clients: claude-code, claude-desktop, cursor,
#    codex, vscode; `tuskd agent setup list` shows status + config paths.
tuskd agent setup claude-code

#    Any other MCP client — stdio:
#      {"command": "tuskd", "args": ["mcp", "--agent", "hermes-dev"]}
#    or streamable HTTP with the printed token:
#      {"url": "http://127.0.0.1:7477/mcp",
#       "headers": {"Authorization": "Bearer <token>"}}

# 5. CLI works alongside the daemon (routed through it, never a second writer)
tuskd status
tuskd search "env parity" --scope project:opentusk

# 6. The loop: review queue + graduation of proven procedures into skills
tuskd graduate
tuskd review list

# 7. Web dashboard — status, search, memories, review, agents, housekeeping
tuskd dashboard          # prints the URL (with a one-per-run operator token)
                         # and opens it; --no-open to just print

# Shut down
kill %1
```

## MCP tools

| Tool | What it does |
|---|---|
| `memory_write` | write a record to your own or a write-granted scope (default `agent:<id>`); supports `supersedes` |
| `memory_search` | hybrid search over entitled scopes; `scopes`, `type`, `tags`, `as_of`, `k ≤ 50`; wildcard grants expand against scopes present in the index |
| `memory_get` | fetch by id (read grant enforced) |
| `memory_promote` | submit a candidate through the gate into `target_scope`; supports `corrects` |
| `memory_reflect` | batch of typed candidates (facts / procedures / corrections), each individually gated — the primary loop entry point |
| `memory_feedback` | `success` / `failure` / `partial` telemetry; feeds ranking and graduation |
| `memory_forget` | hard delete (author or own-scope only) |
| `skill_list` | skills across entitled scopes with trigger + telemetry |
| `memory_status` | your grants, index stats, review-queue depth |

## CLI

```
tuskd init | start | status
tuskd mcp --agent <id>
tuskd agent create <id> [--read s,s] [--write s,s] [--promote s,s]
tuskd agent grant <id> <read|write|promote> <scope> | revoke <id> | list
tuskd agent setup <client> [--agent <id>] [--http] [--print] [--remove] [--yes]
                  # client: claude-code | claude-desktop | cursor | codex
                  #         | vscode | print | list
tuskd agent token rotate <id>
tuskd index [rebuild] | search "<q>" [--scope --as-of --k]
tuskd review list | approve <qid> | reject <qid>
tuskd graduate
tuskd export <archive.tar.gz> | import <archive>
tuskd dashboard [--no-open]
```

## Web dashboard

While the daemon runs it serves an operator dashboard at `/ui` on the same
loopback HTTP listener as `/mcp`: overview (index stats, review-queue depth,
uptime), search with point-in-time `as of` queries, a memories browser
(filter / inspect / forget), review-queue approve/reject, agent management,
and housekeeping (index rebuild, graduation scan, vault export download,
effective config).

Auth is a per-run operator token (`tuskop_…`) minted at daemon start and
written owner-only to `.tusk/admin-token`; `tuskd dashboard` prints the
tokenized URL after checking the daemon is alive. The token travels in the
`Authorization` header (no cookies), agent tokens do not work on `/api/*`,
and every dashboard action goes through the same admin plane as the CLI —
the dashboard cannot bypass scopes, gates, or the single-writer rule.

## Vault layout

```
vault/
├── memory/{agent/<id>, project/<id>, user, org}/*.md   # source of truth
├── skills/<scope-dashed>/<record-id>/SKILL.md          # materialized skills
└── .tusk/{index.db, keyring/agents.json, queue/review.json, tuskd.toml, lock}
```

Configuration lives in `.tusk/tuskd.toml` (HTTP port, per-scope
promotion policies, graduation thresholds, ranking weights). `OPENTUSK_VAULT`
or `--vault` selects the vault.

## Development

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

The full acceptance suite (the 11-step loop over both embedded stdio and HTTP
transports) is `cargo test -p tuskd --test acceptance`. Design decisions made
during the build are logged in `DECISIONS.md`; the authoritative product
contract is `tuskd-rust-spec.md`.

## Releases & the website

Versioning is SemVer; the single source of truth for the version is
`[workspace.package] version` in `Cargo.toml`. To cut and ship a release:

```sh
# 1. bump version in Cargo.toml ([workspace.package])
# 2. build, test, package, and update the manifest:
./scripts/release.sh          # writes site/releases/v<V>/… + latest.json
# 3. publish site + installer + artifacts (needs .env.deploy):
./scripts/deploy-site.sh
# 4. commit — release tarballs are committed so deploys are reproducible
git add -A && git commit -m "release: v<V>"
```

`site/` is the whole public web presence: `index.html` (opentusk.ai docs
page), `install.sh` (served at get.opentusk.ai), and `releases/` (immutable
version dirs + `latest.json`, which the installer and the page's version
badge both read). Hosting details and the rationale for serving artifacts
from Vercel static files (vs. GitHub Releases, for now) are in
`DECISIONS.md` D15.

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
release with `TUSKD_VERSION=v0.1.0`.

**Full documentation lives at [opentusk.ai/docs](https://opentusk.ai/docs)** —
install options, agents & grants, the memory model, the gate, MCP tools, CLI,
dashboard, configuration, and operations. [opentusk.ai](https://opentusk.ai)
is the intro + quickstart; page sources live in the website repo,
[`mindfulagents/opentusk-ai`](https://github.com/mindfulagents/opentusk-ai)
(keep its docs in lockstep with this README when contracts change).

## Quickstart

The guided path — vault, daemon, AI clients, and (optionally) cloud sync
in one interactive command:

```sh
mkdir demo-vault && cd demo-vault
tuskd setup
```

The same steps as plain verbs:

```sh
# (or build from source: cargo build --release; Rust stable — macOS arm64/x86_64, Linux musl arm64/x86_64)
export PATH="$HOME/.local/bin:$PATH"

# 1. Initialize a vault
mkdir demo-vault && cd demo-vault
tuskd init

# 2. Create an agent — the token and MCP configs are printed exactly once.
#    Its ed25519 signing key is stored at .tusk/keyring/keys/<id>.pem
#    (0600, never exported; reserved for signed auth / SEAL / Walrus —
#    use --show-key to print it once instead and keep custody yourself)
tuskd agent create hermes-dev \
  --read project:opentusk,user \
  --write project:opentusk \
  --promote project:opentusk

# 3. Start the daemon (owns index + watcher; serves MCP over HTTP :7477 + UDS)
#    -d detaches it, logging to .tusk/daemon.log; `tuskd stop` shuts it down
tuskd start -d
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

# Shut down (graceful; waits for the vault lock to release)
tuskd stop

# Upgrading later: install the new binary, then cycle the daemon
#   curl -fsSL https://get.opentusk.ai | sh && tuskd restart -d
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
tuskd init | start [-d] | stop | restart [-d] | status
tuskd mcp --agent <id>
tuskd agent create <id> [--read s,s] [--write s,s] [--promote s,s] [--show-key]
tuskd agent grant <id> <read|write|promote> <scope> | revoke <id> | list
tuskd agent setup <client> [--agent <id>] [--http] [--print] [--remove] [--yes]
                  # client: claude-code | claude-desktop | cursor | codex
                  #         | vscode | print | list
tuskd agent token rotate <id>
tuskd agent key path <id>
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
`[workspace.package] version` in `Cargo.toml`. To cut and ship a release
(DECISIONS.md D19):

```sh
# 1. bump version in Cargo.toml ([workspace.package]), merge to main (CI green)
# 2. tag it — the cargo-dist workflow builds all four targets and
#    publishes the GitHub Release with tarballs + sha256 checksums:
git tag v<V> && git push origin v<V>
```

Release config lives in `dist-workspace.toml`; regenerate the workflow with
`dist generate` after editing it. The opentusk.ai website and the
get.opentusk.ai installer live in their own repo
(github.com/mindfulagents/opentusk-ai) and auto-deploy from there — this
repo carries no website (D37). Pre-0.5.0 artifacts remain immutable under
`site/releases/`; hosting history is in `DECISIONS.md` D15, the release
pipeline in D19.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions. See [CONTRIBUTING.md](CONTRIBUTING.md).

**Trademarks:** "OpenTusk", "tuskd", and the mammoth logo are trademarks of
Mindful Agents Lab LLC and are not covered by the code license. The mammoth
mark (`design/logo/`) is adapted from licensed third-party artwork and may
not be redistributed as standalone artwork or used to brand derived projects.

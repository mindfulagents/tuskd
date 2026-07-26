# DECISIONS

Running log of spec-compliant choices made where the spec/build-loop left room, per build-loop §0.

## D1 — Repo root is this directory, not a nested `opentusk/`

Build-loop §1 sketches the tree under `opentusk/`. The working repo is `~/Work/tuskd` and already contains the authoritative docs; nesting a second root adds nothing. The workspace `Cargo.toml` lives at the repo root with the exact `crates/` layout from the spec.

## D2 — `tests/acceptance/` holds the suite; it runs as a `tuskd` test target

The §4 suite file lives at `tests/acceptance/loop_acceptance.rs` per the build-loop tree, wired into `crates/tuskd` via an explicit `[[test]]` path. That placement is required because `CARGO_BIN_EXE_tuskd` (the built binary the suite drives) is only injected into integration tests of the package that defines the binary. `cargo test -p tuskd --test acceptance` runs the suite; plain `cargo test` includes it.

## D3 — MCP protocol is hand-rolled, not `rmcp`

Spec §2.3 allows hand-rolling if rmcp fights ("the protocol is small"). v0 needs exactly: JSON-RPC 2.0 framing, `initialize`/`initialized`, `tools/list`, `tools/call` over stdio (line-delimited) and streamable HTTP (single POST /mcp endpoint returning JSON). Hand-rolling avoids rmcp API churn and keeps the binary small; the tool registry in `tusk-mcp` stays transport-agnostic so rmcp could be swapped in later without touching tool contracts.

## D4 — Frontmatter parser is hand-rolled strict flat grammar

Spec §2.3 prefers "a strict minimal parser (documented grammar) over a heavy YAML dep". Grammar (documented in `tusk-core/src/frontmatter.rs`): `---\n` fence, one `key: value` per line, values are scalars (string / int / float / ISO-8601 string) or flat `[a, b]` arrays; nested maps, multi-line values, and duplicate keys are rejected with a clear error. Strings containing `[`, `]`, `,`, `:` or leading/trailing spaces are double-quoted with backslash escapes on write and unquoted on read.

## D6 — CLI one-shot commands route through the daemon when it is alive

Single-owner rule (spec §2.1 / pitfall 2) forbids a second SQLite writer, but the CLI (`search`, `review`, `graduate`, `agent …`, `index rebuild`, `status`) must work while the daemon runs. Every one-shot command therefore sends an admin request over the daemon's UDS when a daemon holds the vault lock, and only falls back to opening the core embedded (taking the advisory lock itself) when no daemon is running. Agent mutations also go through the daemon so its in-memory keyring never goes stale.

## D7 — Vault path resolution

`--vault` flag > `$OPENTUSK_VAULT` > `./.tusk` exists ⇒ `.` > `./vault/.tusk` exists ⇒ `./vault` > `.` (for `init`). `opentusk init` initializes the current directory as the vault root (memory/, skills/, .tusk/opentusk.toml), matching "initialize a new vault in the current directory".

## D8 — Streamable HTTP is stateless per POST

`POST /mcp` authenticates the bearer token on every request and handles exactly one JSON-RPC message (notifications get 202). No session ids, no SSE stream — a minimal but compliant subset for v0; the seam for stateful sessions is the UDS path, which is session-oriented.

## D9 — Advisory lock is `flock(2)` via the `fs2` crate

`.tusk/lock` is flocked exclusively by whichever process owns the core (daemon, embedded stdio session, or one-shot embedded CLI command). flock dies with the process, so stale locks cannot wedge the vault; the pid is written into the file for diagnostics only.

## D10 — Index rebuild on core open

Both daemon start and embedded open run `index rebuild` (idempotent, spec §3.3) so offline edits made while no watcher was running are always reflected. This also makes "daemon restart preserves search results" hold by construction.

## D11 — memory_write parameter is `content`; reflect type aliases

Tool arg names (not fixed by spec): `memory_write{content, type=episodic, scope=agent:<id>, tags, entities, trust, trigger, version, supersedes}`; `memory_promote{content, type, target_scope, corrects, …}`; `memory_reflect{candidates:[{type, content, scope, …}], target_scope}` where candidate `type` accepts `fact`→semantic, `procedure`→procedural, `correction`→semantic alongside raw record types. `memory_feedback` requires a read grant on the record's scope (the acceptance suite has read-only `claude-code` sending feedback). Graduation provenance is tracked with a `from:<record-id>` tag on the skill candidate so the scanner never re-queues an already-graduated procedure.

## D12 — Binary is named `tuskd` (user-directed)

The spec originally named the shipped binary `opentusk`; the user directed that it be `tuskd` (2026-07-23), and both `tuskd-rust-spec.md` and `tuskd-build-loop.md` were updated to match on the user's instruction. All CLI invocations are `tuskd <command>`. Everything else keeps the original names: the config file is still `.tusk/opentusk.toml`, the vault env var is still `OPENTUSK_VAULT`, tokens are still `tusk_…`, and the vault layout is unchanged.

## D13 — Naming split: OpenTusk is the product, tuskd is the daemon

Ratified 2026-07-23 with the user. Consequences:
- The single binary stays `tuskd` (daemon + its control plane, Consul/Caddy-style); a user-facing `opentusk`/`tusk` CLI is a v1 option via an argv[0]-dispatch symlink — noted, not built.
- Config file renamed to `.tusk/tuskd.toml` (it configures the daemon). Vaults that only have the legacy `.tusk/opentusk.toml` still load via fallback; `tuskd init` writes `tuskd.toml`.
- Kept product-scoped: `OPENTUSK_VAULT` env var, `.tusk/` directory, `tusk_…` token prefix, and "OpenTusk" in prose/branding.

## D14 — Web dashboard is an operator plane on the daemon's existing listener

Added post-v0 at the user's request (2026-07-24). Shape:

- `/ui` is a single embedded HTML file (`include_str!`) — no asset pipeline,
  no Node toolchain; `/api/admin` is a thin bridge that deserializes an
  `AdminRequest` and calls `admin::execute`, the same plane the CLI and UDS
  use (D6), so the dashboard can never bypass the gate/keyring or become a
  second core owner. `/api/meta`, `/api/config`, `/api/export` are the only
  extra read-only endpoints.
- Operator auth is separate from agent auth: a per-run `tuskop_…` token is
  minted at daemon start (only its sha256 stays in memory), written
  owner-only (0600) to `.tusk/admin-token`, advertised in the startup banner
  as `dashboard`, and removed on clean shutdown. Bearer-header auth only —
  no cookies, hence no CSRF surface; the listener stays loopback-bound.
  Agent tokens are rejected on `/api/*`; the operator token is not valid on
  `/mcp`.
- `tuskd dashboard [--no-open]` checks daemon liveness via the UDS socket,
  then prints/opens the URL from the token file.
- New admin verbs `record_list` / `record_get` / `forget` back the memories
  browser; `record_list` pages inside the indexer's existing 500-row browse
  window rather than growing a new query path.
- External contracts unchanged: `/mcp`, `/status`, CLI verbs, tool schemas,
  vault layout, and config keys are untouched (the banner gaining a
  `dashboard` field and the new `.tusk/admin-token` runtime file are
  additive; export already skipped non-vault runtime files).

## D5 — x86_64-unknown-linux-musl toolchain not installed

Per build-loop §0: noted, continuing. No platform-specific code outside `tuskd/src/platform.rs`.

## D15 — Distribution: opentusk.ai docs site + get.opentusk.ai installer, artifacts on Vercel static (2026-07-25)

The public install channel is `curl -fsSL https://get.opentusk.ai | sh`. Because the repo has no public GitHub remote yet, release artifacts are NOT on GitHub Releases; they are static files committed to git under `site/releases/v<version>/` (~2 MB per release) and served by Vercel. This keeps pinned installs reproducible from any clone and needs no extra infrastructure. When the repo goes public, migrate to cargo-dist + GitHub Releases (install.sh's URL scheme was designed to survive that move — only the base URL logic changes) and add Homebrew/cargo-binstall channels.

Mechanics:

- One Vercel project **opentusk-www** (team `team_27HCpCKKFuFwITyEoF34f9u7`) serves `site/` on three domains: `opentusk.ai` (docs page), `www.opentusk.ai` (308 → apex), `get.opentusk.ai` (`/` is a 307 redirect to `/install.sh` — a *redirect*, not a rewrite, because Vercel's filesystem match (index.html) wins over rewrites).
- DNS is DNSimple (account 18748): apex A `76.76.21.21`, `www`/`get` CNAME `cname.vercel-dns.com`. The previous Lovable-hosted page (A `185.158.133.1`) was replaced 2026-07-25; its `_lovable` TXT records were left in place. `docs.`/`app.`/`api.` still point at DigitalOcean apps and were not touched.
- `scripts/release.sh` = quality gate (fmt/clippy/test) → `cargo build --release` → tarball + sha256 → regenerate `site/releases/latest.json`. Version comes from `[workspace.package]` in `Cargo.toml`; releases are immutable (never rewrite an existing `v<version>/` dir).
- `scripts/deploy-site.sh` = link (via Vercel API, credentials in `.env.deploy`, not committed) + `npx vercel deploy site --prod`.
- install.sh verifies SHA-256 before installing, defaults to `~/.local/bin`, never sudo; honors `TUSKD_VERSION` and `TUSKD_INSTALL_DIR`.

## D16 — `tuskd agent setup <client>`: one-command client configuration (2026-07-26)

Additive CLI (spec §7 amended): `tuskd agent setup <client>` writes the MCP
config for a known AI client so the "wire your agent up" step of the quickstart
is one command. Also adds `tuskd agent token rotate <id>` (agent tokens are
stored only as sha256, so HTTP setup for an *existing* agent has to mint a
replacement).

Shape (`tuskd/src/setup.rs`):

- Clients: `claude-code` (`./.mcp.json`, project-scoped), `claude-desktop`
  (`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS,
  `~/.config/Claude/…` elsewhere — path lives in `platform.rs`), `cursor`
  (`~/.cursor/mcp.json`), `codex` (`~/.codex/config.toml`, edited via
  `toml_edit` so comments/formatting survive), `vscode` (`./.vscode/mcp.json`,
  `servers` key + explicit `type`), plus `print` (generic snippet for anything
  else) and `list` (status table).
- **Merge-not-clobber:** only the `opentusk` entry in the client file is ever
  touched; a timestamped `.bak-<epoch>` is taken before any change; malformed
  existing files are refused untouched; unchanged content is a no-op (no
  backup, no write). `--remove` deletes just our entry.
- **stdio by default** — no token in the config, so re-runs are idempotent.
  Config uses the absolute tuskd binary path (GUI clients don't inherit shell
  PATH) and an explicit absolute `--vault` (the client's cwd isn't the vault).
  `--http` embeds a Bearer token (fresh from create, else rotate) and is
  refused for stdio-only clients (claude-desktop, codex).
- Auto-creates the agent (default id = client name) with grants
  `--read org,project:*  --write project:*`, printing the one-time credentials
  block exactly as `agent create` does. `--yes` additionally auto-inits a
  missing vault. `--print` is a pure dry run (no file writes, no agent
  creation, no rotation).
- Every successful setup ends by spawning `tuskd mcp --agent <id>` exactly as
  the client will and driving one MCP initialize — misconfiguration fails at
  setup time, not first use.
- Deliberately does NOT shell out to client CLIs (e.g. `claude mcp add`):
  writing the file is deterministic, testable, and identical across clients.

External contracts unchanged; everything here is additive. Acceptance
coverage: `tests/acceptance/setup_acceptance.rs`.

## D17 — Agent private keys: daemon custody in the vault, export-excluded (2026-07-26)

The ed25519 private key minted at `agent create` was previously printed once
and discarded — dead weight until signed auth exists, and a guaranteed mass
re-keying the day it does. The user's roadmap (SEAL for ACL, ownership of
blobs on Walrus — both Sui, both ed25519-native) makes key custody worth
deciding now.

Decision: **daemon-side custody**, not `~/.ssh`-style home custody. Agents
are vault-scoped principals, and the natural v1+ architecture is the daemon
signing on behalf of authenticated agent sessions (the daemon as ssh-agent);
home-dir custody would force a vault↔key mapping and break single-owner.

- `agent create` (and therefore `agent setup`) now stores the PEM at
  `.tusk/keyring/keys/<id>.pem` (file 0600, dir 0700, via `platform.rs`
  helpers) and prints the *path*, not the key. `--show-key` restores the old
  print-once-store-nothing behavior for client-side custody (e.g. remote HTTP
  agents). `tuskd agent key path <id>` prints the stored location.
- **Export exclusion (the prerequisite):** `archive.rs` skipped only
  `index.db*`/`lock`/`*.sock`, so stored keys would have been tarred into
  every `tuskd export` archive. `skip()` now also excludes
  `.tusk/keyring/keys/**` and `admin-token` (the daemon's per-run operator
  token, which was exportable while a daemon ran). The same skip applies on
  import as defense in depth. `agents.json` still travels — public keys and
  token hashes are the identity roster, not secrets.
- Key **rotation is deliberately deferred**: once keys own on-chain state
  (Walrus blobs, SEAL policies), rotation is a migration, not a file swap —
  inventing local rotation semantics now would prejudge that design.
  Recovery today remains revoke + re-create.
- Keys do not travel in archives, so a vault restored from `tuskd export`
  has agents without stored keys — correct by design; recreate or copy keys
  out of band.

External contracts: additive CLI (`--show-key`, `agent key path`; spec §7
amended); `AgentCreate` admin verb gains an optional `show_key` field
(default false — old callers get the new storing behavior); new runtime
files under `.tusk/` are additive. Coverage:
`agent_keys_are_stored_privately_and_never_exported` in
`tests/acceptance/setup_acceptance.rs`.

## D18 — Daemon lifecycle: `stop`, `restart`, `start --detach` (2026-07-26)

Killing the daemon by PID was the last lifecycle step that required knowing
the internals (and the upgrade flow made that visible). Additive CLI, spec §7
amended.

- `tuskd stop` sends a new `shutdown` verb over the **UDS admin plane** —
  no signals, no PID files. The daemon acks, then triggers the same graceful
  path as SIGTERM (socket + admin-token removed, lock released); `stop`
  waits for the vault lock to release before returning, so the vault is
  immediately reusable. Choosing the admin plane over PID+SIGTERM: no
  stale-PID/PID-reuse hazards, no `unsafe`/libc, and it ports to the
  Windows named-pipe seam. The flock stays the source of truth for
  liveness; the PID in `.tusk/lock` remains informational.
- `tuskd start --detach` (`-d`) re-execs itself in its own process group
  with stdout/stderr appended to `.tusk/daemon.log`, waits up to 10s for
  the socket to answer, prints pid + log path. Plain `start` stays
  foreground (spec contract, launchd/systemd-friendly). `daemon.log` grows
  unbounded — rotation is deliberately out of scope (local dev daemon;
  revisit if it ever matters).
- `tuskd restart [-d]` = tolerant stop, then start. Upgrade flow is now:
  `curl -fsSL https://get.opentusk.ai | sh && tuskd restart -d`.
- Not doing: supervision/auto-restart (service manager's job), PID-file
  liveness, background-by-default `start` (silent contract change).

Coverage: `daemon_lifecycle_detach_status_stop_restart` in
`tests/acceptance/setup_acceptance.rs` (ephemeral HTTP port; kill-guard so a
failed run can't wedge cargo test).

### D18 addendum — v0.4.1: `stop` falls back to SIGTERM for pre-D18 daemons

Found in the field immediately: `tuskd restart -d` after upgrading failed,
because the *running* daemon predates the `shutdown` verb and its admin
parser rejects it (`unknown variant`). v0.4.1: when `stop`/`restart` get
that specific error, they read the pid from `.tusk/lock` and send SIGTERM
via `/bin/kill` (`platform::terminate_pid` — a spawned process, keeping
`#![forbid(unsafe_code)]`), then wait for the lock as usual. Every shipped
daemon version exits cleanly on SIGTERM, so the fallback is safe; it only
triggers on the exact unknown-variant error, so a healthy new daemon is
never signaled. Verified against the archived v0.3.0 binary.

## D19 — Release builds: CI + cargo-dist + GitHub Releases (2026-07-26)

The D15 seam, executed now that the repo is public. Releases are no longer
hand-built on one machine or committed to git.

- **CI gate** (`.github/workflows/ci.yml`): fmt + clippy `-D warnings` + full
  test suite on macos-15 (arm64, the primary target) and
  `cargo check --target x86_64-unknown-linux-musl` on ubuntu, on every PR and
  push to main.
- **Release pipeline** (`.github/workflows/release.yml`, generated by
  cargo-dist — regenerate with `dist generate` after editing
  `dist-workspace.toml`, never by hand): pushing a tag `v<V>` (matching the
  `[workspace.package]` version) builds four targets — aarch64/x86_64
  apple-darwin, aarch64/x86_64 unknown-linux-musl — as `.tar.gz` with
  per-artifact `.sha256`, and publishes a GitHub Release. `pr-run-mode =
  "upload"` builds all targets on PRs touching release config, so target
  breakage surfaces before tagging.
- **Release flow:** bump `[workspace.package] version` in Cargo.toml → merge →
  `git tag v<V> && git push origin v<V>` → CI publishes. `scripts/release.sh`
  is retired for new releases (kept for history); `scripts/deploy-site.sh`
  remains for docs-site changes only.
- **install.sh:** same UX (`curl -fsSL https://get.opentusk.ai | sh`), new
  sources. Latest and v0.5.0+ pins come from
  `github.com/mindfulagents/tuskd/releases` (stable asset names,
  `tuskd-<target>.tar.gz`, no version in the name so
  `releases/latest/download/…` needs no manifest); pins older than v0.5.0
  fall back to the immutable `get.opentusk.ai/releases/<v>/…` archive.
  Existing `site/releases/v*` dirs stay untouched (D15 immutability);
  `site/releases/latest.json` is frozen at v0.4.2 as a legacy artifact.
- Homebrew tap and cargo-binstall channels remain open follow-ups.

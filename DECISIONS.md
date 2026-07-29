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

## D20 — Changelog + release announcements to Buzz (2026-07-26)

Requested by arsy: release notes should reach the team's release-notes Buzz
channel automatically on every release.

- **`CHANGELOG.md`** (workspace root) is now the source of release notes.
  cargo-dist auto-detects it and uses the matching `## v<V> — <date>` section
  as the GitHub Release body (verified with `dist plan`). Update it in the
  same PR as the version bump; seeded retroactively for v0.1.0–v0.5.0.
- **`scripts/announce-release.sh [--dry-run] [<tag>]`** fetches a release's
  notes via `gh release view` and posts them to the Buzz channel named by
  `BUZZ_ANNOUNCE_CHANNEL` with the buzz CLI.
- **Where it runs:** on a machine holding Buzz credentials (operator or the
  team's agent), never in GitHub Actions — the repo is public and Nostr
  private keys stay out of repo and CI secrets. The team agent (Fizz) watches
  for new GitHub Releases and runs the script automatically; the script is
  the manual fallback (`BUZZ_ANNOUNCE_CHANNEL=<uuid> scripts/announce-release.sh`).
## D21 — Sync & encryption model; M0 groundwork: identities + change journal (2026-07-29)

Ratifies the sync design of `PLANS/HOT_CACHE_SYNC_PROPOSAL.md` (v2, approved
2026-07-29) and records the M0 slice that landed here. The product is a paid
hot-cache sync service (DO Spaces `nyc3`) where **the server can never read
memories or keys** — the no-middleman guarantee comes from client-side
encryption, not from any chain.

**Encryption model (per the proposal §4; implementation lands in M1):**
envelope encryption with a per-repo master key. The RMK (32 random bytes,
minted on the first connecting device) is user-facing as a 24-word Repo
Secret Key; each synced object gets its own DEK (XChaCha20-Poly1305), and
DEKs live in a key table wrapped by the RMK inside the repo manifest, so
revocation rotates the small key table — never gigabytes of blobs. New
devices join by Secret Key entry or by device approval (an authorized device
publishes an RMK wrap to the newcomer's ed25519-derived X25519 key).
**Seal-on-Sui is deferred** (proposal Appendix A): client-side encryption
already delivers the confidentiality guarantee; the wrapped-DEK-table layout
stays Seal-ready (adding Seal later = one more wrap of the RMK, no
re-encryption of data).

**M0 groundwork shipped here (proposal §6 items 1–2), all behind
`[sync] enabled` in tuskd.toml, default `false` — zero behavior change for
existing vaults:**

- **Identities.** `vault_id` — a ULID minted on first sync-enabled open,
  persisted at `.tusk/sync/vault_id`, stable across restarts. Device
  identity — an ed25519 keypair minted lazily at `.tusk/sync/device.pem`
  (PKCS#8 PEM, same encoding as agent keys), following the D17 custody
  split: tusk-core mints key material, tuskd persists it (dir 0700, file
  0600 via `platform.rs`) and validates it on every reopen.
- **Change journal with tombstones** (`tusk-core/src/sync.rs`). Every
  VaultStore mutation appends `{op_id (ULID), kind, path (vault-relative),
  content_hash (sha256 of file bytes), ts, prev}` as one JSONL line to
  `.tusk/sync/journal`. Kinds: `put` = full record state (write, offline
  edit), `patch` = metadata mutation (invalidate, telemetry), `tombstone` =
  forget — deletes never vanish without a trace; supersede composes
  put+patch. The file is an append-only **hash chain**: each `prev` is the
  sha256 of the previous line, seeded from a genesis hash bound to the
  vault_id (a journal copied into a different vault fails verification at
  line one). Crash-safety matches the vault's atomic-write posture: one
  `write_all` per line; a torn *final* line is self-healed by truncation at
  open (the lost op is re-derived by reconciliation); corruption anywhere
  else fails hard with `CoreError::Journal`.
- **Reconciliation scan on open** (D10 pattern): `CoreHost::open` — the
  single seam all daemon/stdio/one-shot opens go through — diffs `memory/**`
  on disk against the journal's folded live state and appends `put` for
  new/changed files, `tombstone` for offline deletions. Idempotent.
- **Export boundary:** `.tusk/sync/**` is excluded from `tuskd export`/
  `import` (extends D17): `device.pem` is a secret, and journal/vault_id
  are device-local state a restored vault re-derives via reconciliation.

Deliberately deferred to later slices: the full `[sync]` config section,
`StorageProvider` trait + providers, all crypto/networking, journal coverage
of `skills/**` / review queue / policies (VaultStore mutations cover
`memory/**` today, matching the watcher's scope), journal compaction, and
op signing with the device key. Coverage:
`crates/tusk-core/tests/m0_sync.rs`, `crates/tuskd/tests/m0_sync.rs`.

## D22 — tusk-sync: blocking StorageProvider + client-side crypto layer (2026-07-29)

M0 slice 2 of `PLANS/HOT_CACHE_SYNC_PROPOSAL.md` (§4 encryption design, §6
items 3–4): the new workspace crate `crates/tusk-sync` (MIT/Apache like the
other client crates), holding the storage abstraction and every cryptographic
primitive the sync service uses. No daemon wiring — tusk-core/tuskd are
untouched this slice; the worker, admin verbs, and `[sync]` networking
config come in M1.

**Provider trait is blocking (sync), not async.** Every vault/journal seam
in tusk-core is synchronous `std::fs`; tokio exists in the tree only inside
the tuskd daemon's admin plane, and the M1 sync worker will run on its own
thread (graduation-timer pattern, D6) where blocking I/O is the natural
shape. `reqwest` (blocking) was already a workspace dependency — this slice
adds only its `rustls-tls` feature (pure-Rust TLS; the dep previously had no
TLS backend at all). `StorageProvider: Send + Sync` exposes
`put/get/delete/list` of opaque named blobs with S3-compatible semantics
(idempotent delete, atomic-overwrite put, sorted list). Two impls:

- `LocalProvider` — one file per blob under a root dir, tmp+rename writes
  (tests, offline).
- `SpacesProvider<S: PresignSource>` — bare HTTP verbs against presigned
  URLs handed to it per operation. It holds **no credentials and signs
  nothing**; M1's tusk-cloud is the only party with bucket keys and issues
  the presigns (quota enforcement lives there too). `list` expects a JSON
  array from a control-plane URL — tusk-cloud tracks blobs in its oplog, so
  the client never parses S3 `ListObjectsV2` XML. Tested against a
  hand-rolled local HTTP server on an ephemeral port.

**Crypto (proposal §4), new deps all pure-Rust RustCrypto/dalek:**
`chacha20poly1305 0.10` (XChaCha20-Poly1305), `hkdf 0.12` + `hmac 0.12`
(SHA-256, already in tree), `x25519-dalek 2` (`static_secrets`), `bip39 2`,
`zeroize 1` (derive).

- **RMK** = 32 random bytes (`OsRng`). User-facing **Repo Secret Key** in
  two round-tripping forms: a BIP39 English 24-word phrase (built-in 8-bit
  checksum + wordlist catch typos; parse is case/whitespace-insensitive) and
  a compact `otsk_<64-hex-key><8-hex-sha256-checksum>` string.
- **Subkey discipline:** the raw RMK is never used by two algorithms;
  HKDF-SHA256 subkeys with distinct `info` strings serve key-table wrapping
  and blob naming (a deliberate refinement of the proposal's literal
  `HMAC(RMK, rel_path)` — same property, cleaner domain separation).
- **Per-object DEKs:** random 32 bytes; content sealed with
  XChaCha20-Poly1305 (random 24-byte nonce prepended), `AAD = repo_id ‖ 0x00
  ‖ rel_path`, so a blob decrypts only for the repo and path it was written
  for (wrong-AAD and tamper tests pin this).
- **Key table:** `rel_path → {blob name, DEK}` (JSON), sealed under the
  RMK's key-table subkey as the `manifest` blob. The blob *name* is recorded
  in the table at first write, which is what makes **rotation cheap**: a new
  RMK re-seals the small table and nothing else — no blob is renamed or
  re-encrypted (test-proven), old RMKs can't open the rotated table, and
  post-rotation objects get names under the new RMK.
- **Blob naming:** HMAC-SHA256 of the vault-relative path under the RMK's
  naming subkey, hex — deterministic (overwrites reuse the slot) and
  structure-blind. Server-blindness test audits everything at rest: names
  are opaque hex, stored bytes contain no plaintext/path substrings.
- **Device wraps:** the RMK sealed to a device via ephemeral-static X25519
  ECDH from the slice-1 ed25519 identity (dalek `to_scalar_bytes` /
  `to_montgomery`, the standard birational map), wrap key =
  `HKDF(shared, info ‖ eph_pub ‖ device_pub)`, XChaCha20-Poly1305 with
  `AAD = repo_id`, contributory-DH check on both sides. The serialized wrap
  is safe to relay through the untrusted server.
- **Zeroization:** RMK and DEK zeroize on drop (matching the repo's effort
  level — PEMs elsewhere are plain strings); `Debug` on the RMK never prints
  key material.

Exit shape covered by `crates/tusk-sync/tests/{m0_crypto,m0_roundtrip,
m0_spaces}.rs`: full two-device round trip through a provider (RMK recovered
from the phrase *and* from a device wrap; byte-identical materialization),
server-blindness audit, rotation-touches-only-the-key-table, ciphertext
tamper, wrong AAD, phrase/otsk typo detection.

## D23 — tusk-sync CloudClient: the C4 control-plane contract (2026-07-29)

Client half of tusk-cloud's C4 signing contract (`crates/tusk-sync/src/cloud.rs`):

- **Two signatures, two jobs.** Ops are signed with the device ed25519 key
  over `"tusk-cloud.op.v1" LF repo_id LF hex(sha256(payload))` — the server
  stores and re-serves the signature, so pullers verify authorship
  end-to-end (`verify_op`) without trusting the control plane. Reads sign
  `"tusk-cloud.req.v1" LF method LF path LF timestamp` into `x-tusk-*`
  headers (path excludes the query string; server accepts ±300 s).
- **Golden vectors pinned in both repos** (`tests/m1_cloud.rs` here,
  `tests/c4_vectors.rs` in tusk-cloud): fixed seed key, repo id, payload →
  exact signature bytes. A drift on either side fails a vector test before
  the incompatibility ships. Never regenerate vectors to make a failing
  test pass — that is a protocol version bump, not a bugfix.
- **Server ids are opaque strings** client-side (server-issued lowercase
  hyphenated UUIDs, passed through verbatim) — no uuid dep added; tuskd's
  own identities remain ULIDs (D21).
- **Blocking reqwest** like `SpacesProvider` (worker threads, not an async
  runtime); the only client credential is the device key — no tokens or
  shared secrets. Tests run against a hand-rolled single-shot HTTP server
  (std TcpListener) that verifies every emitted signature server-side; no
  mock-server dependency added.

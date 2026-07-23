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

## D12 — Binary is named `tuskd` (user-directed, supersedes the spec's `opentusk`)

The spec names the shipped binary `opentusk`; the user directed that it be `tuskd` (2026-07-23). All CLI invocations are now `tuskd <command>`. Everything else keeps the spec's names: the config file is still `.tusk/opentusk.toml`, the vault env var is still `OPENTUSK_VAULT`, tokens are still `tusk_…`, and the vault layout is unchanged.

## D5 — x86_64-unknown-linux-musl toolchain not installed

Per build-loop §0: noted, continuing. No platform-specific code outside `tuskd/src/platform.rs`.

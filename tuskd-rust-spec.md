# tuskd — Product Specification (Rust, single-binary)

*The OpenTusk daemon: a local, single-binary memory system for AI agent swarms. Markdown vault, hybrid local search, MCP-native, per-agent keys and grants, and a full intelligence loop (reflect → gate → share → feedback → skill graduation). This spec defines WHAT to build; `tuskd-build-loop.md` defines HOW to build it autonomously.*

**Language:** Rust (stable). **Distribution:** one static-ish binary named `opentusk`. **First target:** `aarch64-apple-darwin` (Apple Silicon). Linux (musl) and Windows follow; no code may assume a platform beyond what `std` + chosen crates abstract.

---

## 1. Product intent

One install gives a machine complete agent memory:

- **Vault:** one Markdown file per memory record in a plain folder — human-readable, diffable, portable. Files are the source of truth; everything else is derived.
- **Index:** embedded SQLite (bundled, FTS5) for hybrid search with metadata + temporal filters. Rebuildable at any time.
- **MCP server:** every agent (Claude Code, OpenClaw, Hermes, custom) talks to the same tool surface over stdio or streamable HTTP.
- **Identity & ACL:** each agent is a principal with an ed25519 keypair and explicit `read/write/promote` grants per scope; every call is authenticated and scope-filtered.
- **Intelligence loop:** scaffolding that turns raw experience into shared, validated, versioned knowledge — promotion gate, review queue, feedback telemetry, and automatic graduation of proven procedures into SKILL.md files agents load natively.
- **Storage seam:** a `StorageProvider` trait so the future Seal/Walrus zero-knowledge sync (M2+) plugs in without touching the kernel. v0 ships `LocalProvider` only.

**Non-goals for v0:** sync/encryption, bundled embedding model (trait exists, provider = none), full web UI (JSON status endpoint only), graph traversal engine.

---

## 2. Architecture

### 2.1 Process model — single-owner daemon (hard requirement)

Exactly one `tuskd` process owns the vault's SQLite index and file-watcher. All sessions reach the core through it:

```
opentusk start                          → daemon: owns index+watcher,
                                          serves MCP-HTTP :7477, /status,
                                          and a Unix domain socket (UDS)
opentusk mcp --agent <id>               → thin stdio⇄UDS proxy to the daemon;
                                          if no daemon is running, runs the core
                                          EMBEDDED in-process (single-user mode)
```

Rationale (learned from the TS prototype): multiple processes sharing SQLite via WAL works but is fragile under watcher storms. Single-owner eliminates the class of bugs. The embedded fallback preserves zero-setup UX for solo use. Even in embedded mode, take an advisory lock file (`.tusk/lock`) so a second embedded instance refuses to start against the same vault; SQLite must still be opened with WAL + `busy_timeout=5000` as defense in depth.

### 2.2 Crate/module layout (one workspace, one shipped binary)

```
crates/
  tusk-core/      vault (records, frontmatter, ULID, bitemporal ops),
                  indexer (SQLite FTS5, search, stats), keyring (ed25519, grants),
                  gate (dedup, contradiction, policy, review queue, graduation),
                  config, StorageProvider + EmbeddingProvider traits
  tusk-mcp/       MCP tool registry bound to an agent identity; transport-agnostic
  tuskd/          the binary: clap CLI, daemon (tokio + axum), UDS server,
                  stdio proxy/embedded mode, status endpoint
```

### 2.3 Dependencies (pin the intent, adjust versions freely)

| Concern | Crate | Notes |
|---|---|---|
| Async runtime / HTTP | `tokio`, `axum` | daemon + streamable HTTP |
| MCP | `rmcp` (official Rust SDK) | stdio + streamable-HTTP server features; if its HTTP server integration fights axum, hand-roll the streamable-HTTP endpoint per MCP spec — the protocol is small |
| SQLite | `rusqlite` with **bundled** SQLite | MUST verify FTS5 at startup (`CREATE VIRTUAL TABLE … USING fts5` probe); if the bundled build lacks FTS5, enable the crate feature that compiles it — failing loudly at boot is required, silently degrading is forbidden |
| Keys | `ed25519-dalek`, `rand`, `sha2` | |
| Serialization | `serde`, `serde_json`, `toml` | config is TOML |
| Frontmatter | hand-rolled parser over `serde_yaml`-style subset OR `gray_matter` | schema is flat; a strict minimal parser (documented grammar) is preferred over a heavy YAML dep |
| IDs | `ulid` | |
| Watcher | `notify` (debounced) | macOS FSEvents backend |
| CLI | `clap` (derive) | |

Binary size target < 15 MB release; `panic = "abort"`, LTO on for release.

---

## 3. Data model

### 3.1 Record

One file: `vault/memory/<scope-path>/<ULID>.md`. Frontmatter (flat) + body:

```
id, type, scope, author, created_at, valid_at, invalid_at,
supersedes?, entities[], tags[], trust (0..1),
uses (int), successes (float), last_used?, version?, trigger?
```

- `type ∈ {episodic, semantic, procedural, skill, profile}`
- `scope ∈ agent:<id> | project:<id> | user | org` → paths `agent/<id>/`, `project/<id>/`, `user/`, `org/`
- **Bitemporal rule:** corrections never edit a body in place. A new record sets `supersedes: <old-id>`; the old record gets `invalid_at = now`. `as_of` queries filter `valid_at ≤ t < invalid_at`.
- `skill` records carry `trigger` (when-to-use, one line) and `version`; body is a complete SKILL.md payload.

### 3.2 Vault layout

```
vault/
├── memory/{agent/<id>, project/<id>, user, org}/*.md
├── skills/<scope-dashed>/<record-id>/SKILL.md     # materialized, agent-loadable
└── .tusk/{index.db, keyring/agents.json, queue/review.json, opentusk.toml, lock}
```

### 3.3 Index (derived, always rebuildable)

Tables: `records` (all frontmatter columns + path) and FTS5 `fts(id UNINDEXED, body, entities, tags)`. `opentusk index rebuild` wipes and re-walks the vault; must be idempotent. Ranking = BM25 combined with telemetry: `score = -bm25 + success_rate + 0.3·ln(1+uses) + 0.2·trust` (constants in config).

---

## 4. Identity, grants, auth

- `agents.json`: id, public key (PEM), sha256 of bearer token, grants `{read[], write[], promote[]}`, created_at, revoked.
- Grant patterns: exact scope or `project:*` wildcard prefix. Agents always implicitly hold read+write on `agent:<own-id>`.
- **stdio:** identity fixed at spawn (`--agent`); refuse unknown/revoked.
- **HTTP:** `Authorization: Bearer <token>` → constant-time hash compare → agent identity per session. (Signed-challenge auth is a v1 upgrade; leave the seam.)
- Every tool call passes through one choke-point function `check(agent, verb, scope) -> Result` — no tool may query the store without it.

## 5. MCP tool surface (names use underscores)

| Tool | Contract |
|---|---|
| `memory_write` | write to own/granted scope; defaults `scope=agent:<id>`; supports `supersedes` |
| `memory_search` | query + `scopes?`, `type?`, `tags?`, `as_of?`, `k≤50`; only entitled scopes; wildcard grants expand against scopes present in index |
| `memory_get` | by id, read-grant enforced |
| `memory_promote` | candidate → **gate** for `target_scope` (needs promote or write grant); supports `corrects` |
| `memory_reflect` | batch of typed candidates (facts/procedures/corrections); each individually gated; the primary loop entry point |
| `memory_feedback` | `id, outcome ∈ {success, failure, partial}`; increments `uses`, adds 1/0/0.5 to `successes`, sets `last_used`; re-ingests to index |
| `memory_forget` | hard delete; only author or own-scope records |
| `skill_list` | skills across entitled scopes with trigger + telemetry |
| `memory_status` | agent grants, index stats, review-queue depth |

Errors: return MCP tool results with `isError`, text `DENIED: <reason>` for ACL, structured JSON otherwise.

## 6. The gate (intelligence-loop kernel)

`submit(candidate, author)`:
1. **Exact dedup:** sha256 of trimmed body vs. all *valid* records in target scope → `rejected_duplicate`.
2. **Near-dup/contradiction:** FTS probe of the candidate against target scope; token-overlap of top hit > 0.7 ⇒ treat as correction (auto-`supersedes`), unless `corrects` given explicitly.
3. **Policy:** per-scope `auto | review`; defaults: `org` = review, `project:*` = auto, `type=skill` = **always review**. Review items persist in `queue/review.json` with qid.
4. Commit path writes record, invalidates superseded, ingests to index. `review approve <qid>` does the same later; approving a skill also **materializes** `skills/<scope>/<id>/SKILL.md` with `name` + `description` (from `trigger`) frontmatter.

**Graduation scanner** (`opentusk graduate`, plus daemon timer, default 24h): valid `procedural` records with `uses ≥ 5 ∧ successes/uses ≥ 0.8` (config `[graduation]`; a distinct-consumers criterion is v1 — record it as a TODO, don't fake it) → wrap body into a skill candidate (tag `graduated`, trigger from first line, provenance footer) → gate (⇒ review queue).

## 7. CLI

```
opentusk init | start | status
opentusk mcp --agent <id>
opentusk agent create <id> [--read s,s] [--write s,s] [--promote s,s]   # prints token + paste-ready MCP configs ONCE
opentusk agent grant <id> <read|write|promote> <scope> | revoke <id> | list
opentusk index [rebuild] | search "<q>" [--scope --as-of --k]
opentusk review list | approve <qid> | reject <qid>
opentusk graduate
opentusk export <archive.tar.gz> | import <archive>
```

Config `opentusk.toml`: vault path, http_port (7477), uds path, `[policies]`, `[graduation]`, `[ranking]`. Env `OPENTUSK_VAULT` overrides vault.

## 8. Platform & distribution

- **Now:** `aarch64-apple-darwin`. Keep everything else compiling: no platform-specific APIs outside a small `platform` module (UDS path defaults, lock semantics; on Windows later: named pipe — isolate behind a trait now).
- Release: `cargo dist`-style pipeline stub; universal-macOS binary is v1; codesign/notarization documented as a release step, not required for local dev.
- Deterministic-ish builds: lockfile committed; `rustls` not OpenSSL if TLS ever appears (it shouldn't in v0).

## 9. Acceptance (product-level)

tuskd v0 is done when the **11-step loop acceptance test** in `tuskd-build-loop.md` passes on Apple Silicon via both transports, `opentusk` is a single binary with no runtime deps, a fresh vault survives `index rebuild` + daemon restart with identical search results, and the README quickstart works verbatim.

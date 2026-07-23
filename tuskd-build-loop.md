# tuskd — Autonomous Build Loop (for Claude Code)

*Operating manual for building tuskd v0 in Rust without human intervention. Read `tuskd-rust-spec.md` first — it is the contract; this file is the process. Work phase by phase; a phase is complete ONLY when its exit tests pass. Never advance with red tests. Never mark the project done until the Acceptance Suite (§4) is fully green.*

---

## 0. Ground rules (apply to every phase)

- **Loop:** plan the phase → write tests for its exit criteria FIRST → implement → `cargo fmt && cargo clippy -- -D warnings && cargo test` → fix until green → commit with message `P<n>: <summary>` → next phase.
- **Target:** develop and test on `aarch64-apple-darwin`. After each phase also run `cargo check --target x86_64-unknown-linux-musl` if the toolchain is available; if not available, note it and continue — but never *introduce* platform-specific code outside `tuskd/src/platform.rs`.
- **No unsafe.** `#![forbid(unsafe_code)]` in every crate.
- **Errors:** `thiserror` types in tusk-core; no `unwrap()`/`expect()` outside tests and `main` bootstrap.
- **Determinism:** all timestamps flow through a `Clock` trait (real + test-fake) so bitemporal tests don't sleep.
- **If a dependency fights you** (e.g., rmcp's HTTP integration), prefer the smallest hand-rolled compliant implementation over wrestling the dep for hours. Document the decision in `DECISIONS.md`. Do not change the spec's external contracts.
- **When ambiguous:** the TS prototype's observed behavior (described in §3 pitfalls and §4 acceptance) is the tiebreaker; otherwise choose the simplest implementation satisfying the spec and record it in `DECISIONS.md`.

## 1. Repo bootstrap (P0)

```
opentusk/
├── Cargo.toml            # workspace: crates/tusk-core, crates/tusk-mcp, crates/tuskd
├── CLAUDE.md             # copy §0 ground rules + "current phase" pointer; keep updated
├── DECISIONS.md
├── crates/...
└── tests/acceptance/     # the §4 suite lives here as integration tests
```

**Exit:** workspace compiles; `opentusk --help` prints command list; clippy/fmt clean; FTS5 boot probe passes (open in-memory rusqlite, create fts5 table — this test exists from day one and must never be removed).

## 2. Phases

### P1 — Vault & records (tusk-core)
Frontmatter codec (strict flat grammar: scalars, `[a, b]` arrays, ISO strings; reject nested maps with a clear error), ULID gen, `VaultStore`: `write / get / load / walk / invalidate / update_telemetry / forget`, scope↔path mapping, scope validation regex, supersession semantics.
**Exit tests:** round-trip serialize/parse property test (arbitrary safe strings); write→get equality; supersede sets `invalid_at` on old + `supersedes` on new; forget removes file; invalid scope rejected; unicode bodies survive.

### P2 — Indexer (tusk-core)
Schema per spec; WAL + busy_timeout pragmas at open; `ingest/remove/rebuild/refresh_path/search/stats`. Search: FTS `OR`-of-quoted-terms; filters scopes/type/tags/as_of/include_invalid; telemetry-boosted ranking per spec formula; empty query = recency listing under filters. Debounced `notify` watcher (≥50ms) feeding `refresh_path`; tolerate partial writes (parse failure ⇒ skip, retry on next event).
**Exit tests:** rebuild idempotent (two rebuilds, identical results); as_of returns superseded record at mid-timestamp and current record at now (fake clock — no sleeps); tag & type filters; ranking test: two lexically-equal records, the one with `uses=7,successes=7` outranks `uses=0`; watcher test: drop a file in, searchable within 1s; delete it, gone within 1s.

### P3 — Keyring & ACL (tusk-core)
`agents.json` persistence; create (returns token + private key PEM exactly once), grant, revoke, `auth_by_token` (constant-time compare on sha256), `can(agent, verb, scope)` with wildcard `kind:*` matching; implicit own-scope rights.
**Exit tests:** wildcard matching table (`project:*` matches `project:x`, not `agent:x`); revoked agent fails auth and all `can()`; token auth constant-time (structural: uses `subtle` or equivalent); own-scope implicit rights present even when grants empty.

### P4 — Gate & loop kernel (tusk-core)
`submit()` exactly per spec §6 (dedup → overlap-0.7 contradiction probe → policy → commit/queue), review queue file ops, `review(qid, approve|reject)`, skill materialization, graduation scanner.
**Exit tests:** duplicate rejected; near-duplicate auto-supersedes (craft >0.7 overlap); explicit `corrects` supersedes; org candidate queues, project candidate commits; skill type always queues regardless of scope policy; approve commits + (skill) materializes SKILL.md with correct frontmatter; reject drains queue without commit; graduation: procedure at uses=7/successes=7 produces exactly one skill candidate, tagged `graduated`; below-threshold produces none.

### P5 — MCP registry (tusk-mcp)
All nine tools per spec §5 against a `TuskContext` + fixed agent id. Every handler calls the ACL choke-point first. DENIED as `isError` text; success = pretty JSON.
**Exit tests (in-process, no transport):** per-tool happy path + at least one ACL denial each; `memory_search` wildcard-grant expansion against index-present scopes; `memory_reflect` batch with mixed scopes returns per-candidate actions; `memory_feedback` math (`partial` = +0.5).

### P6 — Binary: daemon, transports, CLI (tuskd)
clap commands per spec §7; daemon: tokio + axum serving `/status` (JSON) and `/mcp` (bearer→agent→per-session MCP over streamable HTTP), UDS server for local sessions, watcher started once, graduation timer, advisory lock file; `opentusk mcp --agent`: UDS proxy when daemon alive, embedded core otherwise (still honoring the lock); clean shutdown on SIGTERM/stdin-close (watcher stopped, lock released).
**Exit tests:** `/status` 200 with stats; `/mcp` 401 bad token, initialize handshake good token; stdio session against embedded mode completes initialize + one tool call and **exits within 2s of stdin close** (the TS prototype's lingering-child bug — regression-test it); second embedded instance against same vault refuses to start; daemon restart preserves search results.

### P7 — Polish & release lane
`export`/`import` tar.gz of vault; README quickstart (verify by literally executing it); `--version`; release profile (LTO, strip, panic=abort), binary < 15 MB; `DECISIONS.md` complete.
**Exit:** Acceptance Suite green (§4); quickstart transcript committed.

## 3. Known pitfalls (paid for in the TS prototype — do not rediscover)

1. **Lingering stdio processes.** MCP stdio servers must exit when the transport/stdin closes; the watcher will otherwise keep the runtime alive. Tie watcher shutdown to transport close. (Regression test in P6.)
2. **Multi-process SQLite contention.** Watcher storms from two processes produced `database is locked` and a readonly-DB panic. The single-owner daemon design exists because of this. Even embedded mode: WAL + busy_timeout + advisory lock. Never allow two writers.
3. **as_of test traps.** A record created "now" is not valid 60s ago — temporal tests must probe *between* creation and supersession; use the fake clock, never `sleep`.
4. **Watcher partial writes.** File events fire mid-write; parse failures must be swallowed and retried, never crash the indexer.
5. **Dedup scan cost.** v0's exact-dedup may walk the vault; fine to N≈10k records, but put the body-hash into the `records` table from the start so dedup is one indexed query, not a file walk (improvement over the prototype).
6. **FTS5 availability.** Bundled SQLite without FTS5 must fail at boot, loudly. The boot probe test is permanent.

## 4. Acceptance Suite (the definition of done — port of the validated 11-step run)

Integration test, fresh temp vault, two agents:
`hermes-dev` (read: `project:opentusk,user`; promote: `project:opentusk`) and `claude-code` (read: `project:opentusk`; promote: `project:opentusk`). Run once over **embedded stdio** and once over **HTTP against a live daemon** (same assertions):

1. hermes `memory_write` private episodic ("Deploy to staging failed … missing env var WALRUS_EPOCHS") → returns id.
2. claude `memory_search` with `scopes=[agent:hermes-dev]` → **DENIED**.
3. hermes `memory_reflect` with two candidates for `project:opentusk` — a semantic fact (env-parity) and a procedural ("run scripts/envdiff before deploys") → both `committed`.
4. hermes `memory_promote` of the identical procedural text → `rejected_duplicate`.
5. claude `memory_search` "deploy env parity" in `project:opentusk` → hit includes the procedure id.
6. 6× claude + 1× hermes `memory_feedback success` on the procedure → `uses=7, success_rate=1.0`.
7. hermes `memory_promote` correction with `corrects=<fact-id>` → `superseded_existing`.
8. `memory_search` with `as_of` = timestamp captured between steps 3 and 7 → returns the ORIGINAL fact; same query at now → returns the correction, not the original.
9. `memory_status` → totals ≥ 4, grants echoed, queue depth correct.
10. `opentusk graduate` → exactly one review-queue item, `type=skill`, tag `graduated`.
11. `review approve <qid>` → skill committed AND `skills/project-opentusk/<id>/SKILL.md` exists with `name:` and `description:` frontmatter lines.

Plus: unauthorized HTTP `/mcp` → 401; `index rebuild` then repeat step 5 → identical hit; daemon SIGTERM → exits < 2s, lock released.

**Done =** suite green on aarch64-apple-darwin, clippy/fmt clean, no unsafe, quickstart verified. Then stop and report.

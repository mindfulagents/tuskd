# DECISIONS

Running log of spec-compliant choices made where the spec/build-loop left room, per build-loop §0.

## D1 — Repo root is this directory, not a nested `opentusk/`

Build-loop §1 sketches the tree under `opentusk/`. The working repo is `~/Work/tuskd` and already contains the authoritative docs; nesting a second root adds nothing. The workspace `Cargo.toml` lives at the repo root with the exact `crates/` layout from the spec.

## D2 — `tests/acceptance/` is a workspace member crate

Cargo has no root-level free-floating test dirs, so `tests/acceptance` is a member crate (`acceptance`) whose integration tests are the §4 suite. `cargo test -p acceptance` runs the suite; plain `cargo test` includes it.

## D3 — MCP protocol is hand-rolled, not `rmcp`

Spec §2.3 allows hand-rolling if rmcp fights ("the protocol is small"). v0 needs exactly: JSON-RPC 2.0 framing, `initialize`/`initialized`, `tools/list`, `tools/call` over stdio (line-delimited) and streamable HTTP (single POST /mcp endpoint returning JSON). Hand-rolling avoids rmcp API churn and keeps the binary small; the tool registry in `tusk-mcp` stays transport-agnostic so rmcp could be swapped in later without touching tool contracts.

## D4 — Frontmatter parser is hand-rolled strict flat grammar

Spec §2.3 prefers "a strict minimal parser (documented grammar) over a heavy YAML dep". Grammar (documented in `tusk-core/src/frontmatter.rs`): `---\n` fence, one `key: value` per line, values are scalars (string / int / float / ISO-8601 string) or flat `[a, b]` arrays; nested maps, multi-line values, and duplicate keys are rejected with a clear error. Strings containing `[`, `]`, `,`, `:` or leading/trailing spaces are double-quoted with backslash escapes on write and unquoted on read.

## D5 — x86_64-unknown-linux-musl toolchain not installed

Per build-loop §0: noted, continuing. No platform-specific code outside `tuskd/src/platform.rs`.

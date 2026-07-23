# tuskd — build state

## Current phase

**P4 — Gate & loop kernel** (P3 keyring done) (update this marker at each phase transition)

## Ground rules (from tuskd-build-loop.md §0 — apply to every phase)

- **Loop:** plan the phase → write tests for its exit criteria FIRST → implement → `cargo fmt && cargo clippy -- -D warnings && cargo test` → fix until green → commit with message `P<n>: <summary>` → next phase.
- **Target:** develop and test on `aarch64-apple-darwin`. After each phase also run `cargo check --target x86_64-unknown-linux-musl` if the toolchain is available; if not available, note it and continue — but never *introduce* platform-specific code outside `tuskd/src/platform.rs`.
- **No unsafe.** `#![forbid(unsafe_code)]` in every crate.
- **Errors:** `thiserror` types in tusk-core; no `unwrap()`/`expect()` outside tests and `main` bootstrap.
- **Determinism:** all timestamps flow through a `Clock` trait (real + test-fake) so bitemporal tests don't sleep.
- **If a dependency fights you** (e.g., rmcp's HTTP integration), prefer the smallest hand-rolled compliant implementation over wrestling the dep for hours. Document the decision in `DECISIONS.md`. Do not change the spec's external contracts.
- **When ambiguous:** the TS prototype's observed behavior (build-loop §3 pitfalls and §4 acceptance) is the tiebreaker; otherwise choose the simplest implementation satisfying the spec and record it in `DECISIONS.md`.

## Authoritative documents

- `tuskd-rust-spec.md` — the product contract (WHAT). External contracts (CLI, MCP tools, file formats, vault layout) must not change.
- `tuskd-build-loop.md` — the operating manual (HOW), including mandatory pitfall regression tests (§3) and the Acceptance Suite (§4).

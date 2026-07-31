# tuskd — build state

## Current phase

**DONE — all phases P0–P10 complete; Acceptance Suite green** (`tests/acceptance/loop_acceptance.rs`; web-dashboard suite `tests/acceptance/dashboard_acceptance.rs`; agent-setup/key-custody suite `tests/acceptance/setup_acceptance.rs`) (update this marker at each phase transition)

Post-v0 additive CLI (see DECISIONS.md): D16 `tuskd agent setup <client>` + `agent token rotate` (one-command MCP config for claude-code/claude-desktop/cursor/codex/vscode); D17 agent private keys stored at `.tusk/keyring/keys/<id>.pem` (0600, export-excluded, `--show-key` opts out; reserved for signed auth / SEAL / Walrus); D18 daemon lifecycle `start -d` / `stop` / `restart -d` (shutdown verb over UDS admin plane, log at `.tusk/daemon.log`).

## Ground rules (from tuskd-build-loop.md §0 — apply to every phase)

- **Loop:** plan the phase → write tests for its exit criteria FIRST → implement → `cargo fmt && cargo clippy -- -D warnings && cargo test` → fix until green → commit with message `P<n>: <summary>` → next phase.
- **Target:** develop and test on `aarch64-apple-darwin`. After each phase also run `cargo check --target x86_64-unknown-linux-musl` if the toolchain is available; if not available, note it and continue — but never *introduce* platform-specific code outside `tuskd/src/platform.rs`.
- **No unsafe.** `#![forbid(unsafe_code)]` in every crate.
- **Errors:** `thiserror` types in tusk-core; no `unwrap()`/`expect()` outside tests and `main` bootstrap.
- **Determinism:** all timestamps flow through a `Clock` trait (real + test-fake) so bitemporal tests don't sleep.
- **If a dependency fights you** (e.g., rmcp's HTTP integration), prefer the smallest hand-rolled compliant implementation over wrestling the dep for hours. Document the decision in `DECISIONS.md`. Do not change the spec's external contracts.
- **When ambiguous:** the TS prototype's observed behavior (build-loop §3 pitfalls and §4 acceptance) is the tiebreaker; otherwise choose the simplest implementation satisfying the spec and record it in `DECISIONS.md`.

## Distribution, releases, versioning (added 2026-07-25; details in DECISIONS.md D15)

- **Public surface:** the opentusk.ai website and the get.opentusk.ai installer live in `mindfulagents/opentusk-ai` and auto-deploy to DigitalOcean App Platform on merge to that repo's `main` (D37) — this repo carries no website. DNS at DNSimple; credentials in `.env.deploy` (never commit it).
- **Cut a release (D19/D20):** bump `[workspace.package] version` in `Cargo.toml` and add the version's section to `CHANGELOG.md` (same PR) → merge to main (CI gate must be green) → `git tag v<V> && git push origin v<V>` → the cargo-dist workflow builds 4 targets and publishes the GitHub Release with the changelog section as its body. Edit release config in `dist-workspace.toml` and regenerate `release.yml` with `dist generate` — never hand-edit the workflow.
- **Announce (D20):** `scripts/announce-release.sh` posts the GitHub Release notes to the team's release-notes Buzz channel (needs buzz CLI creds + `BUZZ_ANNOUNCE_CHANNEL`; runs on an operator/agent machine, never in CI). An agent-side watcher runs it automatically after each release; the script is the manual fallback.
- **Rules:** shipped releases are immutable (GitHub Release assets and the legacy `site/releases/v<V>/` dirs — never rewrite either); `site/releases/latest.json` is frozen at v0.4.2 (legacy installers only); the website's docs must stay in lockstep with the README quickstart (acceptance criterion: README quickstart works verbatim) — coordinate release PRs with a matching `opentusk-ai` PR.
- **Later:** Homebrew tap + cargo-binstall channels (D19 leaves them open).

## Authoritative documents

- `tuskd-rust-spec.md` — the product contract (WHAT). External contracts (CLI, MCP tools, file formats, vault layout) must not change.
- `tuskd-build-loop.md` — the operating manual (HOW), including mandatory pitfall regression tests (§3) and the Acceptance Suite (§4).

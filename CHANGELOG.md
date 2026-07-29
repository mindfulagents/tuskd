# Changelog

Notable changes to tuskd (the OpenTusk daemon). Each release's section below
becomes the GitHub Release body via cargo-dist and is announced to the team's
release-notes channel (DECISIONS D20). Update this file in the same PR as the
version bump.

## v0.5.0 — 2026-07-26

- **Real release builds (D19):** cargo-dist pipeline publishing tarballs for
  four targets to GitHub Releases; `get.opentusk.ai` installer now pulls
  prebuilt binaries instead of building from source.
- **Dashboard overview redesign:** KPI tiles, activity chart, inline review
  queue.
- **CI gate:** fmt + clippy + full test suite on macOS arm64, plus a
  linux-musl build check, on every PR and push to main.
- **Open-source launch:** repo public under `mindfulagents/tuskd`, dual
  MIT/Apache-2.0 license, CONTRIBUTING with DCO.

## v0.4.2 — 2026-07-26

- Dashboard UI brand restyle to match the new opentusk.ai look.
- Site redesign: mammoth logo system, 4-command quickstart, full `/docs`
  reference page.

## v0.4.1 — 2026-07-26

- Fix: `tuskd stop`/`restart` fall back to SIGTERM when talking to pre-0.4.0
  daemons.

## v0.4.0 — 2026-07-26

- **Daemon lifecycle (D18):** `tuskd stop`, `tuskd restart`, and
  `tuskd start --detach`; log at `.tusk/daemon.log`.

## v0.3.0 — 2026-07-26

- **Agent private keys (D17):** daemon custody at
  `.tusk/keyring/keys/<id>.pem`; keys excluded from export.

## v0.2.0 — 2026-07-26

- **One-command agent setup (D16):** `tuskd agent setup <client>` writes MCP
  client config for claude-code, claude-desktop, cursor, codex, and vscode;
  `tuskd agent token rotate`.

## v0.1.0 — 2026-07-25

Initial build (P0–P10):

- Vault & bitemporal records with frontmatter codec and scopes.
- FTS5 indexer with hybrid ranking, `as_of` filters, debounced file watcher.
- Keyring & ACL: `agents.json`, constant-time token auth, wildcard grants.
- Gate & loop kernel: dedup, contradiction probe, policies, review queue,
  graduation.
- MCP tool registry (nine tools) behind the ACL choke-point.
- `tuskd` daemon: axum `/status` + `/mcp`, UDS transport, stdio proxy, CLI,
  advisory lock.
- Web dashboard with operator auth and embedded UI.
- opentusk.ai docs site + `get.opentusk.ai` installer.

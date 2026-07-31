# Changelog

Notable changes to tuskd (the OpenTusk daemon). Each release's section below
becomes the GitHub Release body via cargo-dist and is announced to the team's
release-notes channel (DECISIONS D20). Update this file in the same PR as the
version bump.

## v0.8.0 — 2026-07-31

- **`tuskd setup` — guided onboarding (D36):** one command that walks
  you from a fresh install to a syncing, client-connected vault. It is
  a checklist, not a questionnaire: every run derives the remaining
  steps from on-disk state, so Ctrl-C and rerun resumes where you
  left off, rerunning on a broken install (expired session, revoked
  device) lands straight on the broken step, and rerunning on a
  healthy install just prints status. Joining from a second device
  gets a numbered repo picker (no UUID copy-paste), the fingerprint
  ceremony with every approval route spelled out, and a wait loop
  that auto-pulls the moment the device is approved. Each step echoes
  the plain command it ran, so the wizard teaches the CLI as it goes.
  Quickstart is now two lines: `curl -fsSL https://get.opentusk.ai | sh`
  then `tuskd setup`.
- **The CLI got a visual design (D35):** tusk-gold and sage brand
  colors, ✓/✗ semantic states, and human-first output across the
  board — `tuskd status` is a readable panel with a sync summary,
  `sync repos` / `review list` / device lists are aligned tables,
  search results are a ranked list with bolded matches, and pull
  shows a single in-place progress line. The recovery phrase now
  renders in a gold box with a shown-exactly-once warning, and the
  device fingerprint appears in the same 4-char gold groups as the
  dashboard's devices page, so cross-checking them is a visual diff.
  Errors are a red `error:` line plus a dim `next:` hint. Everything
  honors NO_COLOR and TTY detection — piped output is byte-identical
  to before, so scripts and integrations are untouched (`--json`
  where you need structure).

## v0.7.3 — 2026-07-31

- **`tuskd sync rename <name>` (D34):** rename a cloud repo — display
  name only; the repo id, devices, and key material never change.
  Defaults to the repo this vault is connected to; `--repo <id>`
  targets any repo you own. The dashboard repo card gained a matching
  rename button (tusk-cloud C15).
- **`tuskd sync init` asks before naming the repo (D34):** on a
  terminal it prompts `Repo name [<default>]:` instead of silently
  using the folder's name, and junk basenames (one letter, `tmp`,
  `test`, …) no longer become repo names by accident — the parent
  folder is prefixed for signal (`projects/a` → `projects-a`).
  Scripts, CI, and `--repo-name` behave exactly as before.

## v0.7.2 — 2026-07-31

- **`tuskd sync connect` no longer requires `--url` (D33):** it uses
  your login session's server, falling back to https://cloud.opentusk.ai
  — so the connect-after-login flow on a second machine is just
  `tuskd sync connect --repo <id>`.

## v0.7.1 — 2026-07-31

- **`tuskd sync delete-repo <id> --yes` (D31):** permanently delete a
  cloud repo you own, freeing your plan's repo slot. Cloud copy only —
  local vaults are never touched.
- **Second-vault boot fix (D32):** when the stock port 7477 is already
  taken (say, by another vault's daemon), `tuskd start` now falls back
  to a free port with a notice instead of dying; custom ports still
  fail hard. `tuskd start -d` failures print the actual error from
  daemon.log, and the auto-sync worker no longer starts before the
  daemon's listeners are up.

## v0.7.0 — 2026-07-31

- **Sign in with your email (D29):** `tuskd sync login` — an 8-character
  code arrives by email, and a 30-day session is stored locally.
  `tuskd sync init` then creates a cloud repo for the vault (with this
  machine as its first approved device) and prints the recovery phrase;
  `tuskd sync repos` lists your repos. Onboarding is now: install →
  `tuskd init` → `sync login` → `sync init` → done. No tokens to copy.
- **The vault syncs itself (D28):** on vaults connected to a cloud repo,
  `tuskd start` runs a background auto-sync worker — incremental,
  oplog-driven push/pull every 30 s (configurable via `[sync]
  interval_secs`; disable with `[sync] auto = false`). Conflicts resolve
  local-wins with re-upload, deletions propagate only for files the
  device itself synced, and key rotations re-key blob names
  incrementally. Manual `sync push`/`pull` now share the same
  incremental engine.
- **Removed:** the beta `sync bootstrap` verb (D30) — replaced by
  `sync login` + `sync init`. Existing repos and devices are unaffected.

## v0.6.0 — 2026-07-30

- **Cloud sync (M1, D21–D27):** end-to-end-encrypted vault sync through
  tusk-cloud (cloud.opentusk.ai). New `tuskd sync` verbs:
  `bootstrap` (create a repo; prints the 24-word recovery phrase once),
  `connect` (enroll this machine as a new device), `status`, `devices`,
  `approve <id> --fingerprint <fp>` (fingerprint-gated device approval),
  `revoke <id>` (one-command revoke + key rotation), `push`, and `pull`.
- **Ciphertext-blind by construction:** every object is encrypted on-device
  under a per-repo master key with per-object DEKs; blob names are opaque
  HMACs; the server stores only public keys, opaque wraps, and ciphertext.
  Op authorship is ed25519-signed and verifiable end-to-end by any device.
- **Key custody:** the recovery phrase or an approved device's wrap are the
  only ways into a repo. Revocation rotates the repo key, re-issues wraps
  to remaining devices, and re-keys blob names on the next push. Devices
  self-heal from their own wrap after rotations performed elsewhere.
- The sync file set is exactly the `tuskd export` file set: private keys,
  sync state, and derived runtime files never leave the machine.

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

# Contributing to tuskd

Thanks for your interest in OpenTusk. A few ground rules keep contributions
smooth.

## Before you start

- Read `tuskd-rust-spec.md` (the product contract — external contracts like
  the CLI, MCP tools, file formats, and vault layout must not change without
  a spec change first) and `DECISIONS.md` (why things are the way they are;
  don't re-litigate a recorded decision without new information).
- The build loop is: write tests for the exit criteria first → implement →
  `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` →
  green before every commit.

## Code rules

- **No `unsafe`.** Every crate carries `#![forbid(unsafe_code)]`.
- **Errors:** `thiserror` types in `tusk-core`; no `unwrap()`/`expect()`
  outside tests and `main` bootstrap.
- **Determinism:** timestamps flow through the `Clock` trait (real + test
  fake) so bitemporal tests never sleep.
- **Portability:** no platform-specific code outside
  `crates/tuskd/src/platform.rs`.
- The full acceptance suite is `cargo test -p tuskd --test acceptance`
  (plus `dashboard_acceptance` and `setup_acceptance`). Integration tests
  spawn real daemons — use the kill-on-drop guard pattern already in the
  suites for any new daemon test.

## Certifying your contribution (DCO)

We use the [Developer Certificate of Origin](https://developercertificate.org/)
instead of a CLA. Sign off each commit (`git commit -s`), which adds a
`Signed-off-by:` line certifying you wrote the change or have the right to
submit it under this project's license.

## License

This project is dual-licensed under either of

- Apache License, Version 2.0 (`LICENSE-APACHE`)
- MIT license (`LICENSE-MIT`)

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.

## Trademarks & logo

"OpenTusk", "tuskd", and the mammoth logo are trademarks of Mindful Agents
Lab LLC and are **not** covered by the code license. The mammoth mark in
`design/logo/` is adapted from licensed third-party artwork and may not be
redistributed as standalone artwork or used to brand derived projects. Forks
must use their own name and logo when distributed publicly.

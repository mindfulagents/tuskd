# The website moved

The opentusk.ai website (landing, /docs, /developers, /cloud) and the
get.opentusk.ai installer live in their own repo:

    https://github.com/mindfulagents/opentusk-ai

That repo auto-deploys to DigitalOcean App Platform on merge to its
`main`. Nothing in this directory is served anywhere — do not add
website files here (D37).

What remains, `releases/`, is the frozen pre-0.5.0 artifact archive
(D15): immutable, produced by `scripts/release.sh` before the
cargo-dist workflow took over at v0.5.0. Never rewrite it.

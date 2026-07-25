#!/bin/sh
# Cut a tuskd release for the current host target.
#
#   scripts/release.sh [--skip-checks]
#
# Reads the version from [workspace.package] in Cargo.toml, runs the quality
# gate (fmt + clippy + tests; skip with --skip-checks), builds --release,
# and produces under site/releases/:
#
#   v<V>/tuskd-v<V>-<target>.tar.gz          the binary, tarred
#   v<V>/tuskd-v<V>-<target>.tar.gz.sha256   checksum (shasum -c format)
#   latest.json                              version + per-target manifest
#
# Artifacts are committed to git and served from get.opentusk.ai by
# scripts/deploy-site.sh. To release a new version: bump the version in
# Cargo.toml, run this script, then deploy. Old release directories are
# never modified — pinned installs stay reproducible.

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

SKIP_CHECKS=0
[ "${1:-}" = "--skip-checks" ] && SKIP_CHECKS=1

VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || { echo "release.sh: could not read workspace version from Cargo.toml" >&2; exit 1; }
TARGET=$(rustc -vV | sed -n 's/^host: //p')

echo "release.sh: tuskd v$VERSION for $TARGET"

if [ "$SKIP_CHECKS" = "0" ]; then
  echo "release.sh: quality gate (cargo fmt / clippy / test) — use --skip-checks to skip"
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
fi

cargo build --release -p tuskd

OUT="site/releases/v$VERSION"
TARBALL="tuskd-v$VERSION-$TARGET.tar.gz"
mkdir -p "$OUT"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT INT TERM
cp target/release/tuskd "$STAGE/tuskd"
tar -czf "$OUT/$TARBALL" -C "$STAGE" tuskd
( cd "$OUT" && shasum -a 256 "$TARBALL" > "$TARBALL.sha256" )

# Rebuild latest.json from everything present for this version
python3 - "$VERSION" <<'EOF'
import json, pathlib, sys
version = sys.argv[1]
rel = pathlib.Path("site/releases")
targets = {}
for sha_file in sorted((rel / f"v{version}").glob("*.tar.gz.sha256")):
    sha, name = sha_file.read_text().split()[:2]
    target = name.replace(f"tuskd-v{version}-", "").replace(".tar.gz", "")
    targets[target] = {
        "url": f"https://get.opentusk.ai/releases/v{version}/{name}",
        "sha256": sha,
    }
manifest = {"name": "tuskd", "version": version, "targets": targets}
(rel / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(f"release.sh: wrote site/releases/latest.json ({', '.join(targets)})")
EOF

echo "release.sh: done — deploy with scripts/deploy-site.sh"

#!/bin/sh
# Post a GitHub Release's notes to a Buzz channel (DECISIONS D20).
#
#   scripts/announce-release.sh [--dry-run] [<tag>]
#
# <tag> defaults to the latest published GitHub Release. Requires:
#   - gh CLI authenticated with read access to the repo
#   - buzz CLI with BUZZ_RELAY_URL / BUZZ_PRIVATE_KEY in the environment
#   - BUZZ_ANNOUNCE_CHANNEL: UUID of the target channel
#
# --dry-run prints the formatted message instead of sending it.
#
# This runs wherever Buzz credentials live (an operator or agent machine),
# not in GitHub Actions — the repo is public and Nostr keys stay out of it.

set -eu

REPO="mindfulagents/tuskd"

DRY_RUN=0
TAG=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    *) TAG="$arg" ;;
  esac
done

if [ -z "$TAG" ]; then
  TAG=$(gh release view --repo "$REPO" --json tagName -q .tagName)
fi

BODY=$(gh release view "$TAG" --repo "$REPO" --json name,body,url,publishedAt \
  --template '{{.name}} — released {{timefmt "2006-01-02" .publishedAt}}

{{.body}}

Release: {{.url}}
Install/upgrade: `curl -fsSL https://get.opentusk.ai | sh`
')

if [ "$DRY_RUN" = "1" ]; then
  printf '%s\n' "$BODY"
  exit 0
fi

[ -n "${BUZZ_ANNOUNCE_CHANNEL:-}" ] || {
  echo "announce-release.sh: BUZZ_ANNOUNCE_CHANNEL is not set" >&2
  exit 1
}

printf '%s\n' "$BODY" | buzz messages send --channel "$BUZZ_ANNOUNCE_CHANNEL" --content -
echo "announce-release.sh: announced $TAG to channel $BUZZ_ANNOUNCE_CHANNEL"

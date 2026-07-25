#!/bin/sh
# Deploy site/ (docs page + installer + release artifacts) to Vercel prod.
#
#   scripts/deploy-site.sh
#
# Serves BOTH hosts from one Vercel project (opentusk-www):
#   opentusk.ai      → site/index.html (docs)
#   get.opentusk.ai  → site/install.sh (via host redirect in site/vercel.json)
#
# Credentials come from .env.deploy (VERCEL_TOKEN). The project is linked
# via site/.vercel/project.json; if missing, this script re-links by asking
# the Vercel API for the opentusk-www project id (creating the project if
# it does not exist). DNS lives at DNSimple — see DECISIONS.md D15.

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

[ -f .env.deploy ] || { echo "deploy-site.sh: .env.deploy not found" >&2; exit 1; }
. ./.env.deploy
: "${VERCEL_TOKEN:?VERCEL_TOKEN missing from .env.deploy}"

PROJECT="opentusk-www"

if [ ! -f site/.vercel/project.json ]; then
  echo "deploy-site.sh: linking site/ to Vercel project $PROJECT"
  INFO=$(curl -fsS -H "Authorization: Bearer $VERCEL_TOKEN" \
    "https://api.vercel.com/v9/projects/$PROJECT" 2>/dev/null) || {
    echo "deploy-site.sh: project not found — creating $PROJECT"
    INFO=$(curl -fsS -X POST -H "Authorization: Bearer $VERCEL_TOKEN" \
      -H "Content-Type: application/json" \
      -d "{\"name\": \"$PROJECT\"}" \
      "https://api.vercel.com/v11/projects")
  }
  mkdir -p site/.vercel
  printf '%s' "$INFO" | python3 -c '
import json, sys
p = json.load(sys.stdin)
json.dump({"projectId": p["id"], "orgId": p["accountId"]}, open("site/.vercel/project.json", "w"))
'
fi

npx --yes vercel@latest deploy site --prod --yes --token "$VERCEL_TOKEN"

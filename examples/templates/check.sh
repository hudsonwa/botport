#!/usr/bin/env bash
# Checks that `botport plan` parses each template spec against a tiny made-up grok-bot-export v1 fixture
# (fixture/<slug>-grok-bot-export.json; no real template content). Usage: check.sh [path/to/botport]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="${1:-$here/../../target/debug/botport}"
tmp="$(mktemp -d)"
trap 'rm -r -- "$tmp"' EXIT
for slug in daily-wisdom roast-my-feed; do
  "$bin" plan --bundle "$here/fixture/$slug-grok-bot-export.json" --spec "$here/$slug/botport.toml" \
    --profile-dir "$tmp/$slug" --json > "$tmp/$slug.json"
  grep -q '"items"' "$tmp/$slug.json" || { echo "FAIL $slug: no plan items"; exit 1; }
  grep -q "^profile = \"tpl-$slug\"" "$here/$slug/botport.toml" || { echo "FAIL $slug: profile is not tpl-$slug"; exit 1; }
  echo "ok $slug"
done

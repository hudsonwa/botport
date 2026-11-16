#!/usr/bin/env bash
# Release gate: the ci.yml run(s) triggered by a push of this commit must all have succeeded.
# Usage: wait-ci-green.sh OWNER/REPO SHA [MAX_WAIT_S]. Exit 0 = green; 1 = failed, cancelled or none found.
# A run still queued or in progress is waited for (every 30 s, up to MAX_WAIT_S, default 3600).
# GH_API (default: gh api) is swappable so scripts/test-wait-ci-green.sh can feed it canned answers.
set -euo pipefail
repo=${1:?OWNER/REPO}; sha=${2:?SHA}; max=${3:-3600}; step=${WAIT_STEP_S:-30}
api=${GH_API:-gh api}
waited=0
while :; do
  # one line per run: "<status> <conclusion>" (conclusion is "null" while running)
  runs=$($api "repos/$repo/actions/workflows/ci.yml/runs?head_sha=$sha&event=push&per_page=20" \
    --jq '.workflow_runs[] | "\(.status) \(.conclusion)"')
  if [ -z "$runs" ]; then
    state=none
  elif grep -qv '^completed ' <<<"$runs"; then
    state=running
  elif grep -qv '^completed success$' <<<"$runs"; then
    echo "FAIL: a ci.yml push run on $sha did not succeed:"; echo "$runs"; exit 1
  else
    echo "PASS: every ci.yml push run on $sha succeeded"; exit 0
  fi
  if [ "$waited" -ge "$max" ]; then
    echo "FAIL: no finished ci.yml push run on $sha after ${max}s (last: $state)"; exit 1
  fi
  sleep "$step"; waited=$((waited + step))
done

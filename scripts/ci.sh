#!/usr/bin/env bash
# Run the same checks as CI: fmt, clippy, tests, and cargo-deny (CI installs it; skipped locally if missing).
# Usage: scripts/ci.sh            (from anywhere inside the repo)
#        CI_MEMCAP="<wrapper> <args> --" scripts/ci.sh   (prefix every cargo call, e.g. a memory cap)
# Exits non-zero on the first failing check.
set -euo pipefail
cd "$(dirname "$0")/.."

read -r -a WRAP <<< "${CI_MEMCAP:-}"

step() {
  echo "==> $*"
  ${WRAP[@]+"${WRAP[@]}"} "$@"
}

step cargo fmt --all --check
step cargo build --workspace --all-targets --locked
step cargo clippy --workspace --all-targets -- -D warnings
step cargo test --workspace

if command -v cargo-deny >/dev/null 2>&1; then
  step cargo deny check licenses advisories
else
  echo "==> cargo-deny not installed; skipping licence/advisory check"
fi
echo "==> all checks passed"

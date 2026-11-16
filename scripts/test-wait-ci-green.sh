#!/usr/bin/env bash
# Tests for wait-ci-green.sh with a fake `gh api` (no network). Run from the repo root.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d); trap 'rm -r "$tmp"' EXIT
fail=0
check() { # name, expected rc, answers file lines (one answer per call, "|" = newline)
  local name=$1 want=$2; shift 2
  printf '%s\n' "$@" > "$tmp/answers"; : > "$tmp/n"
  cat > "$tmp/fakegh" <<'FAKE'
#!/usr/bin/env bash
d=$(dirname "$0"); n=$(wc -l < "$d/n"); echo x >> "$d/n"
line=$(sed -n "$((n + 1))p" "$d/answers"); [ -z "$line" ] && line=$(tail -1 "$d/answers")
[ "$line" = "-" ] || printf '%s\n' "$line" | tr '|' '\n'
FAKE
  chmod +x "$tmp/fakegh"
  GH_API="$tmp/fakegh" WAIT_STEP_S=1 "$here/wait-ci-green.sh" o/r abc 1 > "$tmp/out" 2>&1; rc=$?
  if [ "$rc" != "$want" ]; then echo "FAIL $name: rc $rc, want $want"; cat "$tmp/out"; fail=1; else echo "ok $name"; fi
}
check green 0 "completed success"
check failed 1 "completed failure"
check one-of-two-failed 1 "completed success|completed failure"
check cancelled 1 "completed cancelled"
check waits-then-green 0 "in_progress null" "completed success"
check none-found 1 "-"
exit $fail

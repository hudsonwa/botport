#!/usr/bin/env bash
# Golden-output harness: run a botport binary through plan/apply/drift/sync/export on examples/acme and the committed
# fixtures, then dump every output and the resulting profile trees (temp paths and timestamps normalised).
# Two binaries are byte-identical in behaviour on these inputs when `diff -r OUT_A OUT_B` is empty.
# Usage: scripts/golden.sh BIN OUTDIR      (OUTDIR must not exist; needs python3)
# Runs in a fresh temp HOME; never touches ~/.hermes (every command gets --hermes-home or --profile-dir).
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: $0 BIN OUTDIR" >&2; exit 2; }
BIN=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
OUT=$2
[ -e "$OUT" ] && { echo "$OUT exists" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
W=$(mktemp -d)
W=$(cd "$W" && pwd -P)
trap 'python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1])" "$W"' EXIT
export HOME="$W/home"
export G2H_HOME="$HOME/.g2h"
unset HERMES_HOME XDG_STATE_HOME
mkdir -p "$HOME"

# A stand-in `hermes` for cron create/edit: logs each call next to the profile's cron store.
FAKE="$W/fake-hermes"
cat > "$FAKE" <<'EOF'
#!/bin/sh
[ "$1" = --version ] && { echo 'Hermes Agent v0.22.5'; exit 0; }
prof=$2; shift 3; sub=$1; shift
d="$HERMES_HOME/profiles/$prof/cron"; mkdir -p "$d"
echo "$sub $*" >> "$d/calls.log"
EOF
chmod 755 "$FAKE"

LOG="$OUT/log.txt"
N=0
# run NAME ARGS...: one botport call; stdout, stderr and exit code go to the log, details to details/NAME.json
run() {
  local name=$1; shift
  N=$((N + 1))
  local tag
  tag=$(printf '%03d-%s' "$N" "$name")
  set +e
  "$BIN" "$@" --details "$W/details/$tag.json" > "$W/o" 2> "$W/e"
  local rc=$?
  set -e
  {
    echo "=== $tag: botport $*"
    echo "--- rc $rc"
    echo "--- stdout"; cat "$W/o"
    echo "--- stderr"; cat "$W/e"
  } >> "$LOG"
}

ACME="$REPO/examples/acme"
EXPORT="$REPO/tests/fixtures/grok-export/grok-bot-export.json"
CONF="$REPO/tests/fixtures/conformance/grok-bot"
NOVA="$REPO/tests/fixtures/hermes-migrated/nova"

# full cycle on one source: plan, apply, drift, sync, drift, a Hermes-side edit, drift, apply, --json variants
cycle() {
  local s=$1 prof=$2; shift 2
  local args=("$@")
  run "$s-plan" plan "${args[@]}"
  run "$s-plan-json" plan "${args[@]}" --json
  run "$s-apply-dry" apply "${args[@]}" --dry-run
  run "$s-apply" apply "${args[@]}"
  run "$s-drift" drift "${args[@]}"
  run "$s-drift-json" drift "${args[@]}" --json
  run "$s-sync" sync "${args[@]}"
  run "$s-sync-json" sync "${args[@]}" --json
  run "$s-drift2" drift "${args[@]}"
  # Hermes-side edits: SOUL text, an owned skill, a memory entry, a config key
  if [ -f "$prof/SOUL.md" ]; then
    python3 - "$prof/SOUL.md" <<'EOF'
import sys
p = sys.argv[1]; t = open(p).read()
i = t.find("-->\n")
t = t[:i + 4] + "Edited on the Hermes side.\n" + t[i + 4:] if i >= 0 else t + "Edited on the Hermes side.\n"
open(p, "w").write(t)
EOF
  fi
  local sk
  sk=$(cd "$prof" && find skills -name SKILL.md 2>/dev/null | sort | head -1)
  [ -n "$sk" ] && echo "hermes-side line" >> "$prof/$sk"
  mkdir -p "$prof/memories"
  printf '%s\n§\nThe app restarts on Sundays.\n' "$(cat "$prof/memories/MEMORY.md" 2>/dev/null)" > "$prof/memories/MEMORY.md"
  [ -f "$prof/config.yaml" ] && printf 'hermes_side_key: 1\n' >> "$prof/config.yaml"
  run "$s-edited-drift" drift "${args[@]}"
  run "$s-edited-drift-json" drift "${args[@]}" --json
  run "$s-edited-plan-json" plan "${args[@]}" --json
  run "$s-edited-apply" apply "${args[@]}"
  run "$s-edited-sync-adopt" sync "${args[@]}" --adopt --json
  run "$s-edited-drift2" drift "${args[@]}"
}

# 1. examples/acme with its spec (cron off)
HH1="$W/hh-acme"; mkdir -p "$HH1"
cycle acme "$HH1/profiles/acme-helper" --bundle "$ACME/grok" --spec "$ACME/botport.toml" --hermes-home "$HH1" --no-cron
run acme-export-hermes export-hermes --bundle "$ACME/grok" --spec "$ACME/botport.toml" --hermes-home "$HH1" --out "$W/h2g-acme"
run acme-sync-both sync --bundle "$ACME/grok" --spec "$ACME/botport.toml" --hermes-home "$HH1" --no-cron --direction both --out "$W/h2g-acme2" --json
run acme-drift-both drift --bundle "$ACME/grok" --spec "$ACME/botport.toml" --hermes-home "$HH1" --no-cron --direction both --out "$W/h2g-acme2"

# 2. examples/acme with cron through a stand-in hermes
HH2="$W/hh-acme-cron"; mkdir -p "$HH2"
run acme-cron-plan plan --bundle "$ACME/grok" --spec "$ACME/botport.toml" --hermes-home "$HH2" --hermes-bin "$FAKE" --json
run acme-cron-apply apply --bundle "$ACME/grok" --spec "$ACME/botport.toml" --hermes-home "$HH2" --hermes-bin "$FAKE"
run acme-cron-drift drift --bundle "$ACME/grok" --spec "$ACME/botport.toml" --hermes-home "$HH2" --hermes-bin "$FAKE" --json

# 3. examples/acme without a spec, into a profile dir
cycle acme-nospec "$W/p-acme-nospec" --bundle "$ACME/grok" --profile-dir "$W/p-acme-nospec"

# 4. the grok-bot-export.json fixture into a fresh profile dir
cycle export "$W/p-export" --bundle "$EXPORT" --profile-dir "$W/p-export"

# 5. the grok-bot-export.json fixture onto a `hermes migrate grok-bot` profile
cp -R "$NOVA" "$W/p-nova"
cycle nova "$W/p-nova" --bundle "$EXPORT" --profile-dir "$W/p-nova"

# 6. the conformance hand-export directory
cycle conf "$W/p-conf" --bundle "$CONF" --profile-dir "$W/p-conf"

# 7. export (bundle) and verify from the acme hand export
run export-acme export --out "$W/bundle-acme" --from-dir "$ACME/grok" --agent-data "$W/none" --memory-drop "$W/none" \
  --describe "$ACME/grok/NOT_CAPTURED.md"
run verify-acme verify --bundle "$W/bundle-acme"
HH3="$W/hh-exported"; mkdir -p "$HH3"
cycle exported "$HH3/profiles/acme-helper" --bundle "$W/bundle-acme" --spec "$ACME/botport.toml" --hermes-home "$HH3" --no-cron

# 8. a hand export with a binary skill file
cp -R "$ACME/grok" "$W/bin-src"
sk=$(cd "$W/bin-src/skills" && ls | sort | head -1)
printf '\377\376\000\200' > "$W/bin-src/skills/$sk/logo.bin"
cycle binary "$W/p-binary" --bundle "$W/bin-src" --profile-dir "$W/p-binary"

# dump: logs and every file under the work dir (zip entries listed with digests), temp paths/stamps normalised
python3 - "$W" "$OUT" <<'EOF'
import hashlib, os, re, sys, zipfile
w, out = sys.argv[1], sys.argv[2]
def norm(t):
    t = t.replace(w, "<W>")
    t = re.sub(r"\d{8}T\d{6}Z?", "<STAMP>", t)
    t = re.sub(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z?", "<TIME>", t)
    return t
log = open(os.path.join(out, "log.txt")).read()
open(os.path.join(out, "log.txt"), "w").write(norm(log))
lines = []
for root, dirs, files in os.walk(w):
    dirs.sort()
    for f in sorted(files):
        p = os.path.join(root, f)
        rel = norm(os.path.relpath(p, w))
        if rel in ("o", "e", "fake-hermes"):
            continue
        if f.endswith(".zip"):
            z = zipfile.ZipFile(p)
            lines.append(f"### {rel} (zip)")
            for n in sorted(z.namelist()):
                lines.append(f"  {n} {hashlib.sha256(z.read(n)).hexdigest()}")
            continue
        b = open(p, "rb").read()
        lines.append(f"### {rel} mode {oct(os.stat(p).st_mode & 0o777)}")
        try:
            lines.append(norm(b.decode()))
        except UnicodeDecodeError:
            lines.append(f"<binary sha256 {hashlib.sha256(b).hexdigest()}>")
open(os.path.join(out, "tree.txt"), "w").write("\n".join(lines) + "\n")
EOF
echo "golden: $(grep -c '^=== ' "$OUT/log.txt") runs -> $OUT"

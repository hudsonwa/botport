#!/usr/bin/env bash
# Real end-to-end two-way round trip on a throwaway Hermes profile (never your live profile):
#   fake Grok box (from examples/acme/grok) -> export -> Hermes apply -> the real Hermes bot learns a fact and creates a
#   skill in two chat turns -> export-hermes -> simulated Grok import (AGENT-IMPORT.md) -> re-export -> sync ->
#   both directions in sync, no duplicates, no loop -> CLI battery on the throwaway profile -> profile deleted.
# Opt-in only. Needs a working `hermes` CLI and a configured model. Environment:
#   BOTPORT_E2E_REAL=1          required, otherwise the script skips (exit 0)
#   BOTPORT_E2E_CLONE_FROM      existing Hermes profile to clone the model/provider setup from (required)
#   BOTPORT_E2E_PROFILE         throwaway profile name (default: botport-e2e); it must NOT exist yet (the script refuses
#                               to start otherwise), and only a profile this run created is deleted at the end
#   BOTPORT_BIN                 botport binary (default: botport on PATH)
#   HERMES_HOME                  Hermes home (default: $HOME/.hermes)
#   SKIP_BATTERY=1               skip the battery step
# Usage: BOTPORT_E2E_REAL=1 BOTPORT_E2E_CLONE_FROM=your-bot tests/e2e-real/run.sh OUTDIR
set -u
if [[ ${BOTPORT_E2E_REAL:-} != 1 ]]; then echo "e2e-real: skipped (set BOTPORT_E2E_REAL=1 to run)"; exit 0; fi
: "${BOTPORT_E2E_CLONE_FROM:?set BOTPORT_E2E_CLONE_FROM to an existing Hermes profile}"
OUT=${1:?usage: run.sh OUTDIR}; mkdir -p "$OUT"
P=${BOTPORT_E2E_PROFILE:-botport-e2e}
PD=${HERMES_HOME:-$HOME/.hermes}/profiles/$P
G=${BOTPORT_BIN:-botport}
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EX=$HERE/../../examples/acme
ID=00000000-0000-4000-8000-000000000001
GW=""
CREATED=0
step() { echo; echo "== $*"; }
cleanup() {
  if [[ -n "$GW" ]]; then kill "$GW" 2>/dev/null; wait "$GW" 2>/dev/null; echo "gateway $GW stopped"; fi
  # only ever delete a profile this run created; an existing profile of the same name is never touched
  if [[ $CREATED == 1 ]]; then
    hermes profile delete -y "$P" >/dev/null 2>&1; echo "deleted profile $P: $([[ -d $PD ]] && echo NO || echo yes)"
  else
    echo "profile $P: not created by this run, not deleted"
  fi
}
if [[ -e $PD ]]; then echo "e2e-real: profile $P already exists ($PD); pick another BOTPORT_E2E_PROFILE" >&2; exit 2; fi
trap cleanup EXIT

step "fake Grok box from examples/acme/grok (agent tree + workflows), memory drop attested empty"
BOX=$OUT/box; mkdir -p "$BOX/agent-data/agents/$ID" "$BOX/agent-data/workflows" "$BOX/drop/agents/$ID"
cp "$EX/grok/profile.json" "$EX/grok/settings.json" "$EX/grok/MEMORY.md" "$BOX/agent-data/agents/$ID/"
cp -R "$EX/grok/skills/." "$BOX/agent-data/workflows/"
echo '{"status":"none","date":"2026-10-30"}' > "$BOX/drop/agents/$ID/memory-status.json"
export_box() { "$G" export --agent-data "$BOX/agent-data" --agent-id "$ID" --memory-drop "$BOX/drop" --out "$OUT/grok-bundle" --details "$OUT/export.json"; }
export_box || exit 1
SPEC=$OUT/spec.toml
sed -e "s#^base = .*#base = \"$EX/template\"#" -e "s#^battery = .*#battery = \"$EX/battery.json\"#" \
    -e "s#^profile = .*#profile = \"$P\"#" "$EX/botport.toml" | sed '/^\[hermes_export\]/,$d' > "$SPEC"
printf '\n[hermes_export]\nout = "%s/hermes-export"\n' "$OUT" >> "$SPEC"
A=(--bundle "$OUT/grok-bundle" --spec "$SPEC")

step "throwaway profile (clone of $BOTPORT_E2E_CLONE_FROM) + g2h sync + temporary gateway"
[[ -e $PD ]] && { echo "e2e-real: profile $P appeared during the run; not touching it" >&2; exit 2; }
hermes profile create "$P" --clone-from "$BOTPORT_E2E_CLONE_FROM" --no-alias >/dev/null 2>&1 || exit 1
CREATED=1
rm -f "${PD:?}/SOUL.md" "${PD:?}/memories/USER.md" "${PD:?}/memories/MEMORY.md"
"$G" sync "${A[@]}" --direction g2h --adopt; echo "g2h sync rc=$?"
mkdir -p "$PD/logs"
hermes --profile "$P" gateway run --force >> "$PD/logs/gateway.log" 2>&1 &
GW=$!
for i in $(seq 1 40); do [[ -S $PD/gateway.sock ]] && break; sleep 1; done
"$G" drift "${A[@]}"; echo "drift rc=$? (expect 0: nothing learned yet)"

step "the Hermes bot learns (2 real turns)"
mkdir -p "$OUT/wd"
hermes chat -p "$P" -q "Please save this to your persistent memory now (use the memory tool): the Acme office is closed on public holidays." --in "$OUT/wd" -Q > "$OUT/turn-memory.txt" 2>&1
sleep 3
hermes chat -p "$P" -q "Create a new skill with your skill_manage tool, named holiday-hours, in category ops, description 'use this before answering questions about opening hours'. Body: 1. Check whether today is a public holiday. 2. If so, say the office is closed. Reply with just: done." --in "$OUT/wd" -Q > "$OUT/turn-skill.txt" 2>&1
echo "MEMORY.md mentions holidays: $(grep -c -i holiday "$PD/memories/MEMORY.md" 2>/dev/null || echo 0)"
echo "skill on disk: $(find "$PD/skills" -name SKILL.md -path '*holiday-hours*' | sed "s#$PD/##")"
echo "provenance: $(python3 -c "import json;print(json.load(open('$PD/skills/.usage.json')).get('holiday-hours',{}).get('created_by'))" 2>/dev/null)"
"$G" drift "${A[@]}"; echo "drift rc=$? (expect 1: h2g pending)"

step "Hermes -> Grok: export-hermes (writes the h2g bundle and the sync record)"
"$G" sync "${A[@]}" --direction h2g; echo "h2g sync rc=$?"
"$G" verify --bundle "$OUT/hermes-export"; cat "$OUT/hermes-export/memory/memory.jsonl"; ls "$OUT/hermes-export/normalized/skills" 2>/dev/null

step "simulated Grok import (AGENT-IMPORT.md) + Grok re-export"
python3 -I "$HERE/grok_import_sim.py" "$OUT/hermes-export" "$BOX/agent-data" "$BOX/drop" "$ID"
export_box || exit 1

step "sync both ways again -> converged"
"$G" sync "${A[@]}"; echo "sync rc=$?"
"$G" drift "${A[@]}"; echo "drift rc=$? (expect 0: both directions in sync)"
echo "Hermes-origin skill installed back into acme-grok: $([[ -d $PD/skills/acme-grok/holiday-hours ]] && echo YES-BUG || echo no)"
python3 -I "$HERE/grok_import_sim.py" "$OUT/hermes-export" "$BOX/agent-data" "$BOX/drop" "$ID" | head -1
python3 -c "
import json; rows=[json.loads(l) for l in open('$BOX/drop/agents/$ID/memory-agent.jsonl')]
n=[' '.join(r['fact'].lower().split()) for r in rows]; print('grok facts', len(n), 'duplicates', len(n)-len(set(n)))"

[[ ${SKIP_BATTERY:-0} == 1 ]] && { echo "battery skipped (SKIP_BATTERY=1)"; exit 0; }
step "CLI battery on the throwaway profile"
"$G" test --profile "$P" --battery "$EX/battery.json" --out "$OUT/battery" --details "$OUT/battery/summary.json"
echo "battery rc=$?"

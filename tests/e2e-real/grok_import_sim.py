#!/usr/bin/env python3
"""Simulated Grok bot following docs/AGENT-IMPORT.md on a fake Grok box (the real import needs the bot owner).

usage: grok_import_sim.py HERMES_BUNDLE AGENT_DATA MEMORY_DROP AGENT_ID  -> prints the <=10-line report
Steps as in the brief: facts union by normalized text (tagged "(from Hermes)", source hermes), skills added /
replaced unless the box copy changed since the last sync (here: never), schedules listed as not set up (no tools).
"""
import json
import shutil
import sys
from pathlib import Path


def norm(f):
    t = f.strip().lstrip("-*• ").strip().removeprefix("(from Hermes)").strip().rstrip(".!;, ")
    return " ".join(t.split()).lower()


bundle, agent_data, drop, aid = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4]
mem = drop / "agents" / aid / "memory-agent.jsonl"
mem.parent.mkdir(parents=True, exist_ok=True)
rows = [json.loads(l) for l in mem.read_text().splitlines() if l.strip()] if mem.exists() else []
md = agent_data / "agents" / aid / "MEMORY.md"
have = {norm(r.get("fact", "")) for r in rows} | ({norm(l) for l in md.read_text().splitlines()} if md.exists() else set())
saved = known = 0
for l in (bundle / "memory" / "memory.jsonl").read_text().splitlines():
    r = json.loads(l)
    if norm(r["fact"]) in have:
        known += 1
        continue
    have.add(norm(r["fact"]))
    rows.append({"date": "2000-01-30", "tier": r["tier"], "scope": "hermes", "source": "hermes",
                 "fact": "(from Hermes) " + r["fact"]})
    saved += 1
mem.write_text("".join(json.dumps(r, sort_keys=True) + "\n" for r in rows))
skills = 0
sk = bundle / "normalized" / "skills"
for d in sorted(sk.iterdir()) if sk.is_dir() else []:
    dst = agent_data / "workflows" / d.name
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(d, dst)
    skills += 1
cron = json.loads((bundle / "cron" / "requests.json").read_text())
print(f"Hermes import: {saved} facts saved, {known} already known; {skills} skills added/updated, 0 conflicts")
print("Conflicts: none")
print("Schedules: " + (("not set up: " + ", ".join(j["name"] for j in cron) + " (simulated box has no automation tools)") if cron else "none"))
print("Ignored: none")
print("Next: run the export brief (AGENT-EXPORT.md) so the repo sees the merged state")

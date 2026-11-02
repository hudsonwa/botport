"""``grok-bot`` source for agent-importers (``hermes import-agent grok-bot --source BUNDLE``).

A thin shim: all reading, rendering and conflict rules live in the botport binary, which this
module calls (``BOTPORT_BIN``, else ``botport`` on PATH; the old ``GROK2HERMES_BIN`` and ``grok2hermes`` also work). ``--overwrite`` maps to ``--adopt``
(take over Hermes copies with no sync record; a copy edited after a sync is never overwritten).
A port spec ``botport.toml`` (or the old ``grok2hermes.toml``) inside the bundle is used when present.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path
from typing import Iterator

STATUS = {"create": "imported", "update": "imported", "retire": "imported",
          "in-sync": "skipped", "conflict": "conflict", "skip": "skipped",
          "manual": "skipped", "duplicate": "skipped"}  # manual: a step for the owner; duplicate: not imported twice


def binary() -> str:
    b = (os.environ.get("BOTPORT_BIN") or os.environ.get("GROK2HERMES_BIN")
         or shutil.which("botport") or shutil.which("grok2hermes"))
    if not b:
        raise FileNotFoundError("botport binary not found (set BOTPORT_BIN)")
    return b


def sync_files(source_root: Path) -> Iterator[Path]:
    """Exactly the files the binary reads from a bundle (for the --sync digest)."""
    root = Path(source_root)
    man = root / "manifest.json"
    if man.is_file():
        files = json.loads(man.read_text(encoding="utf-8")).get("files", {})
        yield from (root / f for f in sorted(files) if f.startswith(("normalized/", "memory/")))
        return
    for name in ("profile.json", "settings.json", "MEMORY.md"):
        if (root / name).is_file():
            yield root / name
    if (root / "skills").is_dir():
        yield from sorted(p for p in (root / "skills").rglob("*") if p.is_file())


def run(importer) -> None:
    """Fill ``importer`` (an AgentImporter) with one item per botport plan/apply item."""
    src, target = Path(importer.source_root), Path(importer.target_root)
    cmd = [binary(), "apply" if importer.execute else "plan", "--bundle", str(src),
           "--profile-dir", str(target), "--json"]
    spec = next((f for f in (src / "botport.toml", src / "grok2hermes.toml") if f.is_file()), None)
    if spec:
        cmd += ["--spec", str(spec)]
    if importer.overwrite:
        cmd.append("--adopt")
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
    try:
        report = json.loads(p.stdout)
    except ValueError:
        importer.record("grok-bot", src, target, "error", (p.stderr or "no output").strip()[-300:])
        return
    for it in report.get("items", []):
        dest = target / ("skills/" + it["name"] if it["area"] == "skills" else it["name"])
        status = STATUS.get(it["status"], "error")
        importer.record(f"grok-{it['area']}", src, dest, status,
                        "" if status == "imported" and importer.execute else it.get("detail", ""))

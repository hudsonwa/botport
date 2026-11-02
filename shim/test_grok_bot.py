"""The grok-bot source shim, run inside a patched copy of agent-importers (needs the built binary).

Run: BOTPORT_BIN=target/release/botport python -m pytest shim/ --timeout=60
"""
import importlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
VENDOR = Path(os.environ.get("AGENT_IMPORTERS_SRC",
                             str(HERE.parent / "vendor" / "agent-importers")))


@pytest.fixture()
def pkg(tmp_path):
    if not VENDOR.is_dir():
        pytest.skip("agent-importers source not available")
    dst = tmp_path / "pkgs" / "agent_importers"
    shutil.copytree(VENDOR, dst, ignore=shutil.ignore_patterns("tests", "__pycache__"))
    subprocess.run(["patch", "-p1", "-s", "-i", str(HERE / "agent-importers-grok-bot.patch")], cwd=dst, check=True)
    shutil.copy(HERE / "grok_bot.py", dst / "grok_bot.py")
    sys.path.insert(0, str(dst.parent))
    yield importlib.import_module("agent_importers.importer"), importlib.import_module("agent_importers.sync")
    sys.path.remove(str(dst.parent))
    for m in [m for m in sys.modules if m.startswith("agent_importers")]:
        del sys.modules[m]


def bundle(root: Path) -> Path:
    g = root / "grok"
    (g / "skills" / "alpha").mkdir(parents=True)
    (g / "profile.json").write_text('{"name": "Demo"}')
    (g / "MEMORY.md").write_text("# mem\n")
    (g / "skills" / "alpha" / "SKILL.md").write_text("---\nname: alpha\n---\nA\\nB\n")
    (g / ".env").write_text("SECRET=1\n")
    return g


def test_import_and_sync_digest(pkg, tmp_path):
    importer, sync = pkg
    assert "grok-bot" in importer.SUPPORTED_AGENTS
    src, target = bundle(tmp_path), tmp_path / "home"
    target.mkdir()
    dry = importer.AgentImporter("grok-bot", src, target, execute=False).run()
    assert dry["summary"]["imported"] >= 2 and not (target / "skills").exists()
    rep = importer.AgentImporter("grok-bot", src, target, execute=True).run()
    assert rep["summary"]["error"] == 0, rep
    assert (target / "skills" / "grok-demo" / "alpha" / "SKILL.md").read_text() == "---\nname: alpha\n---\nA\nB\n"
    files = [p.name for p in sync._iter_sync_files("grok-bot", src)]
    assert ".env" not in files and "SKILL.md" in files
    d1 = sync.compute_source_digest("grok-bot", src)
    (src / ".env").write_text("SECRET=2\n")  # never part of the digest
    assert sync.compute_source_digest("grok-bot", src) == d1
    (src / "MEMORY.md").write_text("# mem v2\n")
    assert sync.compute_source_digest("grok-bot", src) != d1
    # an edited Hermes copy is a conflict, not overwritten
    (target / "skills" / "grok-demo" / "alpha" / "SKILL.md").write_text("mine\n")
    (src / "skills" / "alpha" / "SKILL.md").write_text("new\n")
    rep = importer.AgentImporter("grok-bot", src, target, execute=True).run()
    assert rep["summary"]["conflict"] == 1
    assert (target / "skills" / "grok-demo" / "alpha" / "SKILL.md").read_text() == "mine\n"


def test_manual_and_duplicate_items_are_skipped_not_errors(tmp_path, monkeypatch):
    sys.path.insert(0, str(HERE))
    try:
        grok_bot = importlib.import_module("grok_bot")
    finally:
        sys.path.remove(str(HERE))
    items = [{"area": "manual", "name": "MANUAL.md", "status": "manual", "detail": "do X by hand"},
             {"area": "skills", "name": "c/alpha", "status": "duplicate", "detail": "same name twice"}]
    out = subprocess.CompletedProcess([], 0, stdout=json.dumps({"items": items}), stderr="")
    monkeypatch.setenv("BOTPORT_BIN", "unused")
    monkeypatch.setattr(grok_bot.subprocess, "run", lambda *a, **k: out)

    class Imp:
        source_root, target_root, execute, overwrite = tmp_path, tmp_path, False, False
        rows = []

        def record(self, *row):
            self.rows.append(row)

    imp = Imp()
    grok_bot.run(imp)
    assert [r[3] for r in imp.rows] == ["skipped", "skipped"]
    assert [r[4] for r in imp.rows] == ["do X by hand", "same name twice"]


def test_binary_prefers_botport_bin_and_still_accepts_the_old_name(monkeypatch):
    sys.path.insert(0, str(HERE))
    try:
        grok_bot = importlib.import_module("grok_bot")
    finally:
        sys.path.remove(str(HERE))
    monkeypatch.setenv("GROK2HERMES_BIN", "/old/grok2hermes")
    monkeypatch.setenv("BOTPORT_BIN", "/new/botport")
    assert grok_bot.binary() == "/new/botport"
    monkeypatch.delenv("BOTPORT_BIN")
    assert grok_bot.binary() == "/old/grok2hermes"

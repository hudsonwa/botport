# botport — command and file reference

The full command and file reference. For a gentler start, see the [user guide](USER-GUIDE.md).

## Commands

```
botport export  --out DIR [--from-dir DIR | --agent-data /home/box/agent-data] [--describe FILE] [--strict]
botport verify  --bundle DIR
botport describe --bundle DIR [--describe FILE] [--out FILE|-] [--json]   # NOT_CAPTURED.md -> structured describe.json (- = stdout)
botport plan    --bundle DIR --spec botport.toml        # dry run
botport apply   --bundle DIR --spec botport.toml [--adopt]
botport sync    --bundle DIR --spec botport.toml        # apply only when something changed
botport drift   --bundle DIR --spec botport.toml [--no-cron]   # exit 1 on any difference, both directions
botport export-hermes --bundle DIR --spec botport.toml  # Hermes -> Grok: what the Hermes bot learned
botport battery init --bundle export --out battery.json   # starter battery for any bot (USER-GUIDE §6)
botport test    --profile NAME --battery battery.json        # hermes chat --format stream-json + hermes -z
```
Common: `--profile NAME` / `--hermes-home DIR` (default `~/.hermes`) or `--profile-dir DIR`; `--json`; `--details FILE`
(default `$XDG_STATE_HOME/botport/<command>-<profile>.json`, or `<command>.json` with no profile); `-h`/`--help`.
`plan`, `apply`, `sync` and `drift` take `--no-cron` (leave cron jobs out); after `apply --no-cron`, use `drift --no-cron`.
Exit codes: 0 ok, 1 drift/verify/test failure, 2 error (also bare `botport` with no command), 3 apply/sync left conflicts.
`plan`, `apply` and `sync` name the destination profile dir on their first summary line.

## The bundle (`export`, run inside the Grok box)
| Path | What |
|---|---|
| `raw/` | source files verbatim: `agents/<id>/{profile,settings}.json`, `workflows/` skills (managed/plugin skills excluded), memory drop |
| `normalized/` | `profile.json`, `settings.json` (sorted keys), `MEMORY.md`, `skills/<name>/` (literal `\n` unescaped) |
| `memory/memory.jsonl` | the bot's memory drop `{date,tier,scope,source,fact}`, sorted; `memory-status.json` attests "none" |
| `history/messages.jsonl` | read-only snapshot of `search-index.db` (sqlite backup API via `python3`) |
| `NOT_CAPTURED.md` | instructions, schedules, connectors, credentials — with the bot's own description (`--describe`) |
| `normalized/describe.json` | that description parsed: instructions, schedules (cron + IANA timezone), connectors, credential **names**, and any `unparsed` lines (only when a description was supplied); import turns them into paused cron jobs, `mcp_servers` entries and a `botport-manual.md` checklist |
| `manifest.json` | SHA-256 per file + digest; no timestamps |

Secrets are excluded by file name (`.env`, `auth.json`, `*token*`, `*credential*`, keys) and by content (private
keys, `xai-`/`sk-ant-`/`ghp_`… tokens); excluded names are listed in `NOT_CAPTURED.md`. A memory drop older than
`--max-memory-age-days` (2) is a warning, an error with `--strict`. A directory without `manifest.json` is read as a
legacy hand export (`profile.json`, `MEMORY.md`, `skills/` at the top), e.g. [`examples/acme/grok/`](../examples/acme/grok/).

### Compatibility with the old name
botport was called grok2hermes before its first release. The `grok2hermes` command is an alias of `botport`, and the
shim still honours `GROK2HERMES_BIN`. Names from before the rename are still read: `grok2hermes.toml` (any spec path works),
`grok2hermes.lock.json`, `grok2hermes-manual.md`, `<!-- grok2hermes:begin/end NAME -->` SOUL.md markers and export
manifests with `"tool": "grok2hermes"`. `plan`/`drift` treat them as in sync; the next `apply`/`sync` writes the
`botport` names and removes the old lock/checklist (old `backups/grok2hermes/` restore points are left in place).

## The port spec (`botport.toml`)
Says which Hermes-side templates own which parts of the profile. See the made-up example [`examples/acme/botport.toml`](../examples/acme/botport.toml); a real bot keeps its spec in its own repo.
Without a spec: skills + a memory skill under `skills/grok-<name>/` only.

Ownership rules (from agent-importers `sync.py`):
- **skills** — one unit per skill dir under `skills/<category>/`; its tree digest is recorded in
  `<profile>/botport.lock.json` when written. Live differs from both the export and the record → **conflict**,
  never overwritten. No record yet (first run over an existing profile) → conflict until `apply --adopt`.
  A skill that left the export moves to `backups/botport/retired-*`.
- **SOUL.md** — owned sections between `<!-- botport:begin NAME -->` / `<!-- botport:end NAME -->`; text
  outside them is the Hermes side's and survives. A legacy SOUL.md without markers that equals the template is in sync.
- **config.yaml** — only the template's leaf paths are owned (minus `preserve`); lists keep Hermes-side extra items;
  `disable_other_skills` puts every installed skill outside the category into `skills.disabled`.
- **memories/USER.md** — seed entries added if missing, never removed. **cron** — jobs matched by name, written
  through `hermes cron` (real `~/.hermes` only); every job is created paused unless it says `enabled: true`. Unknown keys in a
  cron file are an error. When cron jobs are to be written, `apply`/`sync` first check that `hermes` starts and stop
  before any write if it doesn't (doctor's `fix:` hint; or pass `--no-cron`).
- Every apply first writes `backups/botport/restore-<UTC>.zip` (the owned files); the newest 5 are kept.
- `sync` re-applies only when the bundle digest (over exactly the files read) or the live profile changed.

## Skill allowlist
`skills_include = ["a", "b"]` in the spec imports only those Grok skills (by folder name); `skills_include = []`
imports none (only the memory skill). Excluded skills are invisible: not imported, not in the sync digest (no churn
when they change or vanish upstream), never pending or conflicting, and never sent back by the Hermes → Grok side.
Owned skills a new allowlist excludes are retired to `backups/botport/retired-*`, never deleted.

## Two-way sync (Hermes → Grok)
With `[hermes_export] out = "…"` in the spec, `sync` and `drift` work in both directions (`--direction
both|g2h|h2g`). `export-hermes` writes a bundle of what the Hermes bot learned and the Grok export lacks: memory facts
(union by normalized text, tagged `source: hermes`; anything the Grok side already holds is never sent back), skills
the Hermes bot created or patched (Hermes's own `skills/.usage.json` provenance) or owned skills edited on the Hermes
side (changed on both sides = conflict, never overwritten), and cron jobs as requested actions (`REQUESTS.md`). The
Grok bot loads it by following [`docs/AGENT-IMPORT.md`](AGENT-IMPORT.md); its next export brings the merged
state back, and skills that came from Hermes are never installed back as copies. Persona/SOUL/instructions only ever
flow from the repo.

## agent-importers source
`shim/` adds a `grok-bot` source to NousResearch agent-importers: `shim/agent-importers-grok-bot.patch` (4 small
edits) + `shim/grok_bot.py`, which calls this binary. Then `hermes import-agent grok-bot --source BUNDLE`
and `hermes import-agent --sync` work. Test: `uv run --with pytest --with pyyaml python -m pytest shim/`.

## g2h: live chat between the Grok Bot and the Hermes bot
`crates/g2h` builds `g2hd` (a local broker on a Unix socket, no network listener) and `g2h` (its CLI). The Grok Bot
calls `g2h ask/send/wait/recv` through its computer connector; the Hermes bot calls `g2h hermes-send`; when the
Grok side is idle, g2hd wakes it through a Grok routine webhook (debounced, capped, `dry_run` by default).
```
g2h setup [--hermes-home P] [--profile NAME] [--yes]   # idempotent wizard: config, credentials (key hidden), user service
g2h setup --check                                       # ok/FAIL per item with the fix command
g2h selftest [--live]                                   # round trip + dry-run wake; --live fires ONE webhook (asks first)
g2h uninstall [--keep-logs]                             # reverses setup-manifest.json
```
The home is `~/.g2h` on every platform, found by CLI, daemon and setup with no env var (`G2H_HOME` overrides;
`g2h home` prints it; an old-default home is moved there on first run).
Point your agent at [docs/AGENT-SETUP.md](AGENT-SETUP.md); prompts for the Grok side are in
[docs/ROUTINE-PROMPT.md](ROUTINE-PROMPT.md); threat model in [docs/SECURITY.md](SECURITY.md).
Release binaries: each GitHub release has one `botport-<target>.tar.gz` per target (with `botport`, `grok2hermes`, `g2h` and `g2hd`; macOS arm64/x86_64, Linux x86_64/aarch64 musl, static) plus a `.sha256`; to build them locally, `scripts/build-release.sh`.


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

### Routine check (`--check-routines PATH|auto`, experimental)
Opt-in on `verify`, `plan`, `apply`, `sync` and `drift` (any other command refuses the flag). It compares the
routine names in the export with the routines the Grok desktop app's local cache has seen (its automation-changed
events) and prints a `WARN` line naming each routine the cache knows but the export left out.

**What it reads:** this reads your Grok desktop app's local conversation transcripts (the `transcript.replicas`
files) in full, on this machine only. It keeps only the names of routines from automation events. Nothing else is
stored, copied or printed, and nothing is sent anywhere.

It never changes the exit code and never drops the command's report: a missing cache, a refused path, a failed
command or an unreadable export adds one `routine check (experimental): skipped (<reason>)` line, and a cache file
that isn't JSON is a `WARN` and is skipped. Its lines come right after the first summary line. The 10-line summary limit still applies, so with many missing
routines some of them (and some of the command's own lines) only appear in the details file; `routine_check` there
always lists every missing name.

Which files are opened, and nothing else:
- Only regular files named `<base32>.blob` whose base32 name decodes to a key containing `.transcript.replicas.`.
  A file that is a symlink is never opened.
- `PATH` a file: opened only if it passes that rule. Anything else (`sand-secrets.json`, `Cookies*`, a webhook-key
  blob, any other file, or a symlink) is refused unopened with a `skipped` note.
- `PATH` a directory: listed (not recursively); only its files that pass the rule are opened. A symlinked directory is
  refused when given as written; with a trailing `/` or `/.` the operating system follows the link, and its entries
  are then still opened only if they pass the rule above.
- `auto`: that directory at the app's default location, `<app data>/Grok Bot/sand-client-persistence`, where
  `<app data>` is `~/Library/Application Support` (macOS), `%APPDATA%` (Windows) or `$XDG_CONFIG_HOME`/`~/.config`
  (Linux).

Cache file names (they carry an account key) are never printed. The report JSON gets a `routine_check` block (counts
and the missing names).

Experimental because the event format is assumed, not confirmed against a real cache: each file is JSON or JSON
lines, and any object whose `type`, `event` or `kind` is `automation-changed` (or `automation_changed`,
`automationChanged`) names a routine at `automation.name`, `payload.name` or `name`; a change with `deleted: true`
or `action`/`change` of `deleted`/`removed` forgets it. Names match ignoring case and punctuation. If your cache
looks different, the check finds nothing rather than guessing.

## The bundle (`export`, run inside the Grok box)
| Path | What |
|---|---|
| `raw/` | source files verbatim: `agents/<id>/{profile,settings}.json`, `workflows/` skills (managed/plugin skills excluded), memory drop |
| `normalized/` | `profile.json`, `settings.json` (sorted keys), `MEMORY.md`, `skills/<name>/` (literal `\n` unescaped) |
| `memory/memory.jsonl` | the bot's memory drop `{date,tier,scope,source,fact}`, sorted; `memory-status.json` attests "none" |
| `history/messages.jsonl` | only with `--with-history` (off by default; nothing on the Hermes side reads it): read-only snapshot of `search-index.db` (sqlite backup API via `python3`); `--history-db`/`--history-sqlite` need `--with-history` |
| `NOT_CAPTURED.md` | instructions, schedules, connectors, credentials — with the bot's own description (`--describe`) |
| `normalized/describe.json` | that description parsed: instructions, schedules (cron + IANA timezone), connectors, credential **names**, and any `unparsed` lines (only when a description was supplied); import turns them into paused cron jobs, `mcp_servers` entries and a `botport-manual.md` checklist |
| `manifest.json` | SHA-256 per file + digest; no timestamps |

Every bundle file (this one and the `export-hermes` bundle) is written `0600` and its folders `0700`: memories, facts,
the bot's description and any history are personal data. When `export` replaces an old export in `--out`, it removes
only the regular files the old manifest lists, reached through real folders inside `--out`; it never follows a symlink,
and an entry it leaves in place is named in the error.

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

Two botport files sit in the profile and are easy to confuse:
- `botport.lock.json` is the **sync record** (what was written and sent each way). Don't delete it: without it every
  owned unit reads as a conflict until `apply --adopt`.
- `.botport.lock` is the **operation lock**: an OS file lock held while a botport run writes into the profile, so two
  runs never interleave (see [SECURITY.md](SECURITY.md)). It holds only the last holder's pid. It is released when
  that run exits; never delete it to "unlock" a profile.

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
`shim/` adds a `grok-bot` source to NousResearch agent-importers **older than 1.2.0** (1.2.0 and later have their own
official `grok-bot` importer, and installing the shim there would replace it; don't): `shim/agent-importers-grok-bot.patch` (4 small
edits) + `shim/grok_bot.py`, which calls this binary. Then `hermes import-agent grok-bot --source BUNDLE`
and `hermes import-agent --sync` work. Test: `AGENT_IMPORTERS_SRC=<agent-importers checkout, older than 1.2.0> uv run --with pytest
--with pyyaml python -m pytest shim/ -rs`. The integration test needs that upstream source, which is not in this repo: without it it
is **skipped** (CI has no copy, so CI skips it; the two unit tests still run; `SHIM_REQUIRE_SRC=1` makes a missing source a failure).

## g2h: live chat between the Grok Bot and the Hermes bot
`crates/g2h` builds `g2hd` (a local broker on a Unix socket, no network listener) and `g2h` (its CLI). The Grok Bot
calls `g2h ask/send/wait/recv` through its computer connector; the Hermes bot calls `g2h hermes-send`; when the
Grok side is idle, g2hd wakes it through a Grok routine webhook (debounced, capped, `dry_run` by default).
```
g2h setup [--hermes-home P] [--profile NAME] [--yes]   # idempotent wizard: config, credentials (key hidden), user service
g2h setup --check                                       # ok/FAIL per item with the fix command
g2h selftest [--live]                                   # round trip + dry-run wake; --live fires ONE webhook (asks first)
g2h uninstall [--keep-logs]                             # removes the files setup writes (manifest is a hint)
```
The home is `~/.g2h` on every platform, found by CLI, daemon and setup with no env var (`G2H_HOME` overrides;
`g2h home` prints it; an old-default home is moved there on first run).
Point your agent at [docs/AGENT-SETUP.md](AGENT-SETUP.md); prompts for the Grok side are in
[docs/ROUTINE-PROMPT.md](ROUTINE-PROMPT.md); threat model in [docs/SECURITY.md](SECURITY.md).
Release binaries: each GitHub release has one `botport-<target>.tar.gz` per target (with `botport`, `grok2hermes`, `g2h` and `g2hd`; macOS arm64/x86_64, Linux x86_64/aarch64 musl, static) plus a `.sha256`; to build them locally, `scripts/build-release.sh`.


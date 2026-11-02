# Ownership: what botport writes, and when it refuses

This is the spec of botport's core: how it decides, per item, whether it may write into a Hermes profile. It describes
the code as it is in `src/adapters/hermes/sync.rs` (Grok -> Hermes) and `src/adapters/hermes/twoway.rs` (Hermes ->
Grok); function names are given so each rule can be checked. If this page and the code disagree, the code wins and this
page has a bug.

## Per-item states
`plan` turns the wanted state (the model plus the port spec, built by `want`) and the live profile into a list of items.
Each item has an area (`skills`, `soul`, `config`, `mcp`, `user`, `cron`, `manual`, `export`) and one state (`St` in
`src/core/adapter.rs`):

| State | Meaning | Written by `apply`? |
|---|---|---|
| `in-sync` | live already equals what is wanted | no (but its record is refreshed) |
| `create` | missing in the profile | yes |
| `update` | wanted changed, and the live copy still equals the record of what botport last wrote (exceptions: see below) | yes, except the `export` item |
| `retire` | an owned skill gone from the export, live copy untouched since last written | yes: moved to `backups/botport/retired-<stamp>/` |
| `conflict` | live differs from what is wanted **and** from the record (edited on the Hermes side), or there is no record | no; with `--adopt`, only items flagged `unrecorded` (see below) |
| `duplicate` | the same skill also under `skills/grok-bot-imports/` (from `hermes migrate grok-bot`) | no; neither copy is deleted |
| `skip` | not managed here: cron when cron is not managed (see Cron below); in Hermes -> Grok, a fact, skill or cron request with credential-like text, left out | no |
| `manual` | needs a human; listed in `botport-manual.md` | no (only the checklist file is) |

The writing rule is `should_write`: an item is written when its state is `create`, `update` or `retire`, or when it is a
`conflict` flagged `unrecorded` and `--adopt` was passed; the `export` item is never written (`apply` filters `area !=
"export"`). A conflict that has a record is never written, with or without `--adopt`. Not every conflict without a
record is flagged `unrecorded`: a live skill unit the export no longer has, malformed SOUL markers, a name used by two
cron jobs, and a Hermes-origin skill changed on both sides stay conflicts even with `--adopt`.

Two `update` items do not fit the "equals the record" meaning: the `export` item from `plan_source` (the source changed
since the last run; informational, never written), and the manual checklist (`plan_manual`, no record). Spec cron jobs
are the third exception, below.

## The lock record (`<profile>/botport.lock.json`)
Struct `Lock`. The old name `grok2hermes.lock.json` is still read and is migrated by the next write (`migrate_legacy`,
`write_lock`).

| Field | Area | Holds |
|---|---|---|
| `bundle_digest`, `files_read` | source | digest of the source and of each file read; `plan_source` lists what changed |
| `skills` | skills | skill unit (`<category>/<name>`) -> tree digest of all its files |
| `soul` | SOUL.md | section name -> sha256 of the text between `<!-- botport:begin NAME -->` / `end` markers; `__file__` -> sha256 of the whole SOUL.md |
| `config` | config leaves | dotted leaf path (only the template's leaves, minus `preserve`) -> hash of the value botport wants there |
| `config` (`mcp_servers.<id>`) | MCP | connector id -> hash of its owned leaf values |
| `g2h_cron` | cron from the bot's schedules | job name -> digest of prompt + schedule |
| `h2g_skills`, `h2g_history`, `h2g_seen`, `grok_excluded`, `h2g_cron` | Hermes -> Grok | skills sent to the Grok side (latest and every version), sent skills later seen in a Grok export, skills ever excluded by the allowlist, cron requests already sent |

There is no record for the USER.md seed, for the spec's own cron jobs, or for the manual checklist (see below).

## Transitions per area (`plan`)
- **Skills** (`plan_skills`, `plan_unit`). Live missing: `create` (or `in-sync`, "adopted", when the same skill is
  already under `skills/grok-bot-imports/`, or when it is the memory unit and every fact is already in Hermes memory).
  Live equals wanted: `in-sync`. Live equals the record: `update`. Record exists, live differs from it: `conflict`. No
  record: `conflict` (unrecorded). A live unit in the owned category that the export no longer has: `retire` if it
  equals the record, else `conflict`.
- **SOUL.md** (`plan_soul`). No file: `create`. Unbalanced or duplicated markers: one `conflict` for the whole file.
  With markers: each wanted section is `create` (missing), `in-sync`, `update` (live section equals its record) or
  `conflict`. Without markers, when the spec has no SOUL template and only the persona's instructions are wanted
  (`soul_append`): each section is `in-sync` if its text is already in the file, else `create` (appended). Without
  markers otherwise: `in-sync` if the file equals the template, `update` if it equals the `__file__` record, else
  `conflict`.
- **Config leaves** (`plan_config`, `config_item`). Only leaves of the spec's template, minus `preserve`, are owned,
  plus `skills.disabled` when the spec sets `disable_other_skills` (added even if the template lacks it). A list keeps
  the live list's extra items (`config_target`, `merge_list`), except `skills.disabled`, which is owned whole. Missing:
  `create`; live equals the record: `update`; recorded and differs: `conflict`; unrecorded and differs: `conflict`
  (unrecorded). Every other value in config.yaml is kept, but the file is not kept byte for byte: any config write
  re-emits the whole document from the parsed value with the template's header comment (`plan_config`; `apply_mcp` keeps
  only the live leading `#` lines). Comments the owner wrote inside config.yaml (other than that header), quoting and
  formatting are lost on the first apply that changes an owned leaf or MCP entry.
- **MCP** (`plan_mcp`). Same rule over the leaves of `mcp_servers.<id>`; a live server with the same url under another
  name counts as `in-sync` ("adopted").
- **USER.md seed** (`plan_user`). `create` when any seed entry is missing; entries are appended, never removed or edited
  (`apply_user`).
- **Cron from the bot's schedules** (`plan_model_cron`). Matched by name. Missing: `create`, always created paused
  (`schedules_to_cron` sets a paused reason on every job). Live equals wanted: `in-sync`. Exists with no record:
  `in-sync`, adopted as it is. Live equals the `g2h_cron` record: `update`; else `conflict`. Two or more jobs with the
  name: `conflict`.
- **Cron from the port spec** (`plan_spec_cron`, `spec_job_item`). Matched by name, **no record**: a live job whose
  prompt, deliver or schedule differs from the spec is an `update`, so the spec wins. It is a `conflict` only when the
  job is paused live but the spec does not ask for it paused, or when the name is used twice. A spec job may be created
  enabled or paused as the spec says. Paused jobs `hermes migrate grok-bot` made (`<Bot>: <routine>`) that the spec and
  the schedules do not name are `in-sync` ("adopted"), whether or not cron is managed.
- **Manual checklist** (`plan_manual`). Rewritten when the list changes; ticks on unchanged lines are kept
  (`apply_manual`).
- **When cron is managed** (`real_home` in `src/main.rs`): cron is written only through the `hermes` CLI, so it is
  managed only for `--profile`/`profile` in `~/.hermes`, or in another HERMES_HOME when `--hermes-bin` is given, and
  never with `--no-cron` or `--profile-dir`. Otherwise cron items are `skip`, except a bot-schedule job whose name
  already exists live (adopted as `in-sync`) and the migrate jobs above.

## Commands
- **`plan`**: the list above. Read-only; takes no lock.
- **`drift`**: the same plan, read-only, no lock. Exit 1 when any item is not `in-sync`, `skip` or `manual` (the
  checklist file aside). With `--direction both` it also runs `twoway::collect` read-only; with `--direction h2g` it
  runs only that Hermes -> Grok half (`h2g_report`), not the plan.
- **`apply`** (`sync::apply`; read it for the exact sequence): takes the operation lock if the profile dir exists, plans
  under it; if there is something to write and no lock was taken (a profile dir that does not exist yet), it creates the
  dir, locks it and plans again. It writes the restore point only when there is something to write or legacy file names
  remain, then writes in a fixed order (`migrate_legacy` when legacy names remain, `apply_skills`, `apply_soul`,
  `apply_config`, `apply_user`, `apply_mcp`, `apply_manual`, `record_model_cron`, `apply_cron`, `write_lock`). Exit 3
  when conflicts remain.
- **`sync`**: `apply`, except that nothing is written when the source digest equals the record, no item is
  `create`/`update`/`retire` and no legacy file names remain. With `--direction both` one operation lock covers both
  halves.
- **`--adopt`**: an unrecorded conflict is written and becomes `update` ("adopted ->", `mark_done`); from then on it has
  a record like any other item.

## What gets recorded
A unit is recorded only when it is ours after the run: `in-sync`, or just written (`apply_skills`, `apply_mcp`,
`apply_config`, `record_model_cron`). A conflict that stays a conflict (`kept_conflict`) keeps its old record or none,
so the next plan still sees the Hermes-side edit. Retired skills lose their record.

**The SOUL-lock rule** (`record_soul`): each marked section is recorded unless it is in conflict; `__file__` is recorded
only when no SOUL item is in conflict, and is removed when any is. So a hand-edited marked section, or a hand-edited
marker-less SOUL.md, is never treated as "untouched since last write".

## "Never overwrite a Hermes-side edit", precisely
For skills, marked SOUL sections, a marker-less SOUL.md, owned config leaves, MCP entries and cron jobs from the bot's
schedules: when the live copy differs from both what is wanted and the recorded digest, the item is a `conflict` and
`apply`, `sync` and `apply --adopt` leave it as it is. Within SOUL.md and config.yaml the other items are still written
(`apply_soul` keeps a conflicting section's live text; `apply_config` sets only the changing paths on top of the live
file). Text outside SOUL markers, config values outside the owned leaves or under `preserve`, extra list items, and
existing USER.md entries are never botport's (their values are kept; config.yaml comments are not, see below).

The exceptions, stated plainly:
- the spec's own cron jobs have no record: a Hermes-side edit to their prompt, deliver or schedule is overwritten by the
  spec (a paused job is the exception, see above);
- `skills.disabled` is owned whole when the spec sets `disable_other_skills`;
- an `unrecorded` conflict is overwritten when you pass `--adopt`; that is what the flag is for;
- config.yaml comments and formatting are not kept when botport writes the file (see Config leaves);
- updating a skill unit removes files in it that the export no longer has (only when the unit is `update`, i.e.
  untouched since botport wrote it).

## Hermes -> Grok (`twoway.rs`)
`collect` builds the bundle for the Grok side: facts the Grok side does not already hold (`norm`, `union_new`), skills
the Hermes bot created or edited, and cron differences as requested actions. A skill changed on both sides since the
last sync is a `conflict`: reported, not exported. `record` writes `h2g_skills`, `h2g_history`, `h2g_seen` and
`h2g_cron` into the same lock file, under the same operation lock. Persona, SOUL and instructions are never exported.

## Restore point and operation lock
- **Restore point** (`restore_point`): before the first write of an `apply`/`sync` that has something to write, a zip of
  `config.yaml`, `SOUL.md`, `memories/USER.md`, `cron/jobs.json`, the checklist, the lock file and every file under
  `skills/<category>/` goes to `backups/botport/restore-<stamp>-NN.zip` (mode 0600; the newest 5 are kept). If any of
  those files holds something that looks like a secret, no restore point is written and nothing is applied. An error
  after this point names the restore point and says earlier changes in the run were written.
- **Operation lock** (`OpLock`, `src/core/util.rs`): an OS file lock (`flock`) on `<profile>/.botport.lock`. `apply`,
  `sync` and the Hermes -> Grok record take it before they plan; a second writer fails fast naming the holder. A
  read-only profile dir with no lock file runs without one (`try_acquire` returns `None`, with a note). The kernel
  releases it when the process exits. `plan` and `drift` never take it.

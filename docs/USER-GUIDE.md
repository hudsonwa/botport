# botport — user guide

botport copies a **Grok Bot** into a **Hermes** profile and keeps the two in sync. It is one program with two
halves:
- **Export** runs inside the Grok Bot's box. The bot does this itself when you send it the agent brief.
- **Import** runs on your machine (macOS or Linux). It shows what would change, applies it safely, and later checks for drift.

Your Hermes-side edits are never overwritten (they show up as conflicts), secrets are never exported, and every
apply writes a restore point first.

## 1. Export a bot (≈ 5 minutes, in the Grok chat)
In the chat of the bot you want to export, attach these files (from a checkout of this repo and its GitHub release):
- `docs/AGENT-EXPORT.md` (the brief the bot follows)
- the release archives `botport-x86_64-unknown-linux-musl.tar.gz` and `botport-aarch64-unknown-linux-musl.tar.gz`
  (the bot picks the right one)
- optionally `SHA256SUMS` from the same release (the bot checks the archive against it before extracting)

and send: **"Follow the attached AGENT-EXPORT.md and export yourself."**

The bot asks at most a couple of questions (only if it can't work them out), writes its memory and a description of
its instructions/schedules/connectors, runs the export, and sends you back `<name>-g2h-bundle.tgz` with two short
summaries.

**Several bots at once:** send the same files to one bot with *"Follow the attached AGENT-EXPORT.md and export
these bots: helpdesk, sales, …"* (or *"all bots"*). It coordinates the others; if it can't message them itself, it gives you
one line to paste into each of their chats. You get one bundle per bot.

## 2. Check the bundle
Unpack the bundle into a folder of your choice. Below it is `~/your-bot/grok` (keeping it under git is a good idea,
so you can see what changed between exports):
```bash
mkdir -p ~/your-bot/grok && tar -xzf /path/to/your-bot-g2h-bundle.tgz -C ~/your-bot/grok --strip-components=1
```
```bash
botport verify --bundle ~/your-bot/grok
```
`verify` checks every file against the bundle's SHA-256 manifest. Read `NOT_CAPTURED.md` in the bundle: it lists what
Grok can't export (instructions, schedules, connectors, credential names) with the bot's own description of each. To get it as structured data (schedules as cron + IANA timezone, connectors,
credential names only — never values):
```bash
botport describe --bundle grok --out describe.json
```
Lines it can't classify are listed under `unparsed` (never dropped or guessed); schedules without a timezone are flagged,
and schedules the bot says run on another machine are `side: other` (context, not to recreate). When the export had a
description, the bundle already carries the same file as `normalized/describe.json`.

**Descriptions are copied as text, so never put secret values in them.** Secrets are never exported on purpose: a
write guard refuses known secret shapes. In short: a schedule, connector or credential line that mentions a secret
keyword (`token`, `api key`, `password`, …) is not imported but listed on the manual checklist, and any written text
that looks like a real secret (`sk-…`, `api_key=abc123…`) is refused or replaced by `[text not copied: <reason>]`.
The full rules are in [SECURITY.md](SECURITY.md#botport-porter-secret-detect-and-route).

Clean text is copied byte-identical. It is a detector, not a guarantee. describe.json has no `raw` field; only structured fields are kept. Inside `## Schedules`, a line
such as "None on the Grok side" or "Web side, for context:" makes the list lines after it `side: other`, until the
next heading or blank line.

### What the export can't copy (and what import does with it)
`plan`/`apply` read the description (and, for a grok-bot-export file, its routines and apps) and handle each item:

| Described item | On the Hermes side |
|---|---|
| schedule with a time, an IANA timezone and a `prompt:` | a Hermes cron job named `<Bot>: <routine>`, created **paused** (`enabled: false`; the prompt is copied byte-identical; the timezone is in its paused reason: check the host's timezone, then `hermes cron resume`). A job of that name that already exists (e.g. from `hermes migrate grok-bot`) is adopted, never doubled; a Hermes-side edit is a conflict, not overwritten. |
| schedule without a timezone, time or prompt; event-triggered routine; "runs on another machine" | `manual` item |
| MCP connector with an `http(s)` URL | owned `mcp_servers.<id>` leaves in `config.yaml` (`url`, `tools.include`), under the same ownership/conflict rules as the config template; an existing entry with the same URL is adopted |
| built-in / login-based connector, MCP without URL | `manual` item |
| schedule or connector that looks like it holds a secret (prompt, name, URL or tools) | `manual` item with the reason; nothing of it is written to Hermes |
| credential (name only) | `manual` item — set it yourself (e.g. the profile's `.env`); no value is ever copied |

Credential parsing is an allow-list: the name must be identifier-shaped (`NAME`, `` `name` ``, `**name**`) and not
look like a token (>= 16 chars mixing letters and digits, or `ghp_`/`xox*-`/`AKIA`/`sk-`...). Only the name is kept:
any other text on the line (a location too) is dropped with a warning that names the credential only. A name of the
plain shape `[A-Za-z][A-Za-z0-9_.-]{0,63}` with an unknown all-lowercase part of 6+ letters (`PW_zqhunterpass`) may
embed a value: it goes to the checklist without the name. Write names only in the description: a short value written like a name (`**Hunter2x**`)
cannot be told apart, and a short all-letters value in a URL path (`.../mcp/hunter`) cannot be detected, so keep both out.

`manual` items are listed in `<profile>/botport-manual.md` (`- [ ]` per item, names only), written by `apply`
behind the restore point. Tick boxes as you go: ticks are kept and are not a change. `manual` is not a difference, so
`drift` stays IN SYNC once everything else is. Cron is managed through the `hermes` CLI, so only for `~/.hermes` (or a
home given with an explicit `--hermes-bin`, which gets `HERMES_HOME`); elsewhere jobs are `skip`.

## 3. Describe the target profile (once per bot)
A small spec file tells botport where the bot goes and which Hermes templates to render. It lives in the bot's
repo next to its templates. For a new bot copy the made-up example
`examples/acme/` (bundle, templates, spec, battery) into the bot's repo and edit:

| Key | Meaning |
|---|---|
| `profile` | Hermes profile name to create/update |
| `base` | folder holding your templates (relative paths below resolve against it) |
| `skill_category` | Hermes skill folder for the bot's Grok skills (e.g. `acme-grok`) |
| `skills_include` | optional allowlist of Grok skill folder names to import; `[]` = none (Grok's shared library skills stay out) |
| `[memory_skill]` | the bot's memory becomes a skill with this name (+ optional header file) |
| `[soul] template` | SOUL.md template; botport owns marked sections, your other text is kept |
| `[config] template`, `preserve`, `[config.inherit]` | config.yaml template; `preserve` lists keys other tools own (never touched); `inherit` copies a key's value from another YAML file (e.g. `web:` from another profile's config.yaml) |
| `[vars]` | `{{NAME}}` substitutions; built in: `{{SPEC_DIR}}`, `{{HERMES_HOME}}` (e.g. `REPO = "{{SPEC_DIR}}/../.."`) |
| `[user] seed`, `[cron] file` | USER.md seed and scheduled jobs to create |
| `battery` | test questions for `botport test` |

Without `--spec`, botport imports just skills and memory, into the profile named by `--profile NAME`.

## 4. Import
A bot repo can wrap the commands below in its own scripts with the right paths.
```bash
cd ~/your-bot && B=grok S=botport.toml
```
```bash
botport plan --bundle $B --spec $S
```
`plan` is read-only: one line per change (create / update / retire / conflict). When it looks right:
```bash
botport apply --bundle $B --spec $S
```
(If other tools also edit the Hermes config, wrap this in whatever lock they use.)
- The **first** apply onto a profile that already exists (for example one built by hand) needs `--adopt` once,
  so botport records which files it owns.
- If the summary says *restart needed*, restart that bot's gateway when it is idle:
  `hermes -p your-bot gateway restart`. Hermes only picks up new MCP tools on a restart.
- Exit code 3 means conflicts: files you edited on the Hermes side. They are left alone and listed in the details file.

## 5. Keep in sync
After each new export from the Grok side:
```bash
botport sync --bundle $B --spec $S
```
`sync` does nothing (instantly) when neither the export nor the profile changed. To only check:
```bash
botport drift --bundle $B --spec $S
```
Exit 0 = in sync, 1 = different (cheap enough to run from cron).

## 5b. The other way: Hermes → Grok
If the spec has `[hermes_export] out = "…"`, `sync` also writes what the Hermes bot learned
since: new facts, skills it created or edited, cron jobs it runs. `drift` prints both directions in one line, e.g.
`drift your-bot: g2h IN SYNC (12 items, …); h2g 2 pending (1 fact(s), 1 skill(s), 0 cron)`.
To hand it to the Grok bot: `tar -czf your-bot-hermes-bundle.tgz -C ~/your-bot hermes-export`, attach it
with `docs/AGENT-IMPORT.md` in the bot's chat and send **"Follow the attached AGENT-IMPORT.md."** It reports in ≤ 10
lines. Then export it again (section 1) so the repo sees the merged state; h2g shows IN SYNC after that (until
then `drift` exits 1 with "h2g … pending": expected, it means the Grok bot hasn't loaded the bundle yet).
Facts are only ever added: a fact reworded on one side arrives as a second fact on the other.
Rules: facts are merged without duplicates and never echo back; a skill changed on both sides is a conflict on both
sides (nothing overwritten); persona/SOUL never travels between the bots.

## 6. Test the bot
Start from a generated battery, edit it, then run it against the profile:
```bash
botport battery init --bundle export --spec botport.toml --out battery.json
# or straight from an upstream grok-bot-export v1 file:
botport battery init grok-bot-export.json --out battery.json
```
The path (positional or `--bundle`) is a bundle directory or a `grok-bot-export` v1 JSON file; anything else is an
error. From an export, memory cases come from the bot's memories and there are no schedule/connector cases.
```bash
botport test --profile my-bot --battery battery.json
```
`battery init` is deterministic (same bundle, same file) and writes, each marked `"generated": true` with a `why`:
a persona question (expects the first word of the bot's name), one case per imported skill (the spec's
`skills_include` applies), one per schedule/connector the description parser finds in `NOT_CAPTURED.md` (bullets under a matching heading when it finds none), up to
three memory facts (from `memory/memory.jsonl`, else `MEMORY.md` bullets), and live-status questions that must call
a tool. Tighten the patterns, drop cases that don't fit, add your own.

`test` asks each question through `hermes chat` and prints a pass/fail table (≤10 lines, failures first); every
answer, tool list and reason is in the results JSONL it names. A turn stopped by the tool-turn limit fails with
"hit the tool-turn limit after N tool calls"; its forced summary is still scored. Exit 0 only when all pass.

Battery schema (JSON):

| key | meaning |
|---|---|
| `gap_s` | seconds between cases (default 3) |
| `gateway` | optional `{"prompt", "expect"}`: one `hermes -p NAME -z` turn whose answer must equal `expect` |
| `cases[].id`, `family`, `prompt` | required; `--only id,id` picks cases |
| `cases[].want` | any-of tool names or prefixes that must be called; `"*"` = any tool; `[]` = none needed |
| `cases[].forbid` | tool names/prefixes that must not be called |
| `cases[].patterns` / `absent` | regexes (case-insensitive) that all must / none may match the answer |
| `cases[].generated`, `why` | notes for the owner; ignored by `test` |

## 7. Undo
Every apply/sync first zips what it will touch to `~/.hermes/profiles/<name>/backups/botport/restore-*.zip`
(last 5 kept): config.yaml, SOUL.md, USER.md, cron jobs and the bot's skill folder. To roll back, stop the
profile's gateway and unzip the newest one over the profile (files the apply newly created are not removed by
this; `plan` afterwards lists them):
```bash
cd ~/.hermes/profiles/your-bot && ls -t backups/botport | head -1
```
```bash
unzip -o backups/botport/<that file> -d ~/.hermes/profiles/your-bot
```

## Output and tokens
Every command prints at most 10 summary lines (`--json` for machines) and puts full detail in a file
(`--details FILE`). Agents should read the summary, not the bundle. Exit codes: 0 ok, 1 drift/verify/test failure,
2 error, 3 conflicts left.

## For agents on the Hermes side
An agent asked to "import / sync bot X" should: run `verify`, then `plan`, show the owner the summary, run `apply`
only after the owner agrees (or if already told to), restart the gateway only when
idle, then run `test`. Never pass `--adopt` without the owner's OK, and never delete restore points.

## Coming from `hermes migrate grok-bot`

Upstream Hermes ships its own Grok Bot importer (`hermes migrate grok-bot` / `hermes import-agent grok-bot`). Its input is a
`grok-bot-export.json` the Bot writes about itself. botport works with both sides of it:

- **Its input.** Anywhere `--bundle DIR` is accepted (`verify`, `plan`, `apply`, `sync`, `drift`, `export-hermes`), you can
  pass the export file instead: `botport plan --bundle ~/Downloads/grok-bot-export.json --profile NAME`. It is
  detected by its `"format": "grok-bot-export"` field. `verify` checks it parses, is version 1 and has no secret-looking
  keys, and warns about facts the Bot marked `[truncated …]` (they are kept as written). Skill texts stored with literal
  `\n` are unescaped. An export with several bots uses the first one.
- **Pasted exports.** The export can come straight from the chat: `--bundle -` reads it from stdin and
  `--clipboard` from the clipboard (`pbpaste` on macOS; `wl-paste` or `xclip` on Linux, else an error). Give only
  one input: `--bundle` with another value, or twice, is refused. For the run it is saved as
  `pasted-grok-bot-export.json` (mode 0600) in a new `paste-<random>` dir (mode 0700) under botport's state dir
  (`$XDG_STATE_HOME/botport`, default `~/.local/state/botport`), read from there, and removed when the command ends. A run killed by Ctrl-C, SIGTERM or a crash skips that cleanup and can leave its `paste-<random>` dir (holding the bot's memories) under that state dir; later runs never read it, and it is safe to delete by hand when no botport command is running. Strict JSON is read exactly as before; otherwise botport tolerates, and
  reports on stderr one line per repair:
  - the JSON inside a fenced code block (```` ```json ````, ```` ``` ````, `~~~` or a longer fence, also all on one
    line) with chat text around it, or with no fence the text from the first `{` to the last `}`;
  - a reply split across several messages: the line break before each closing fence is dropped and all fenced blocks
    are joined in order with nothing between them, so a split inside a text value or a number joins up; the result
    must parse. If two or more blocks are each a complete export (an object with `format` or `bots`), it is refused as
    ambiguous; if exactly one is, that one is taken. Paste the parts in the order the bot sent them;
  - trailing commas before `}` or `]`, outside string values (commas inside text are kept).

  Smart quotes are refused, not repaired. If a chat app curled the JSON quotes (“ ” ‘ ’), botport can't tell
  for sure where each value ends, so a paste that still does not parse and holds a curly quote is refused with
  "the export was mangled by the chat app (smart quotes); ask the bot to re-send it as a file or a plain code
  block". Do that: a file, or a plain code block, keeps the straight quotes. Curly quotes inside straight-quoted
  values (`"say “hi” now"`) are ordinary text and are read as they are. After any repair, botport also counts the
  memories, user facts, skills and routines in the pasted text and refuses the paste if the parsed export holds a
  different number.

  The secret-key check runs on the repaired document, so a credential inside a fenced block is still refused (and
  nothing is saved). The token check runs on the pasted text and again on the repaired document, so a token split
  across two fragments is refused once joined.
- **Shared skills.** Grok Bot skills come from a library shared by all your Bots, so every export carries all of them.
  Pick the ones this profile takes with `skills_include = [...]` in the spec (`skills_include = []` takes none).
- **Its output.** On a profile the upstream importer already filled, `plan`/`drift`/`apply` treat its work as present:
  a skill in `skills/grok-bot-imports/<name>/` counts as installed (no second copy in your skill category); paused cron
  jobs named `"<Bot>: <routine>"` are adopted as in sync; if every memory fact is already in `memories/MEMORY.md` or
  `USER.md`, the memory skill is not created.
- **Duplicates.** A skill present both in your skill category (`skills/<skill_category>/<name>/`) and in
  `skills/grok-bot-imports/<name>/` is reported as a `duplicate` item with both paths. botport never deletes either
  copy; remove the one you don't want by hand. `drift` counts a duplicate as a difference.

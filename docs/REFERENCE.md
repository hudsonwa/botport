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
botport digest write  --hermes-home PROFILEDIR [--pair NAME] [--window-days 7] [--owner-user ID]... [--titles]
botport digest show   --file F [--pair P --key F]
botport digest inject --hermes-home PROFILEDIR [--mode full|pointer] [--max-age-days 14] [--peer-file F] [--owner-user ID]...
botport mirror routines --bundle export --profile NAME [--tz UTC]   # routine parity, Grok vs Hermes (exit 1 on a difference)
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

## Mirror mode (`mirror`)
A Grok Bot and a Hermes bot kept in step as equal peers (design: [design/MIRROR.md](design/MIRROR.md)). Each setup
item (persona name and instructions, each skill, routine and connector) is compared with its own base on each side;
a change on one side is carried to the other, a change on both sides to one item goes to a pick-one list, and
nothing is overwritten silently.

```
botport mirror init --pair P (--profile NAME [--hermes-home DIR] | --profile-dir DIR [--hermes-bin PATH])
                    --transport local|drive|hybrid [--local-dir D [--bot-key K]]
                    [--drive-dir D | --drive-folder-id ID --token-cmd CMD] [--category C]...
                    [--approve runnable|all] [--key-file F | --no-pair-key] [--copy-one-sided] [--force]
botport mirror init --undo --pair P
botport mirror plan|status --pair P
botport mirror sync --pair P [--allow-retire] [--trigger F] [--take-over]
botport mirror watch --pair P [--interval 1s] [--every 5m] [--for DUR] [--now]
botport mirror check --pair P [--reset]
botport mirror show|approve --pair P ITEM [--yes]
botport mirror pick --pair P ITEM (--keep grok|hermes | --local-only | --forget) [--yes]
botport mirror restore --pair P ITEM --from SHORT_DIGEST
botport mirror stats [--since 7d] [--metrics FILE]
botport mirror applied --pair P ITEM                    # a listed change was applied in the Grok app by hand
botport mirror setup --print-charter|--print-skill     # the Mirror bot's charter; the box-wide mirror-recent skill
botport mirror digest-items --pair P (--agent-id ID [--agent-data DIR] [--with-skills] | --from EXPORT) --out DIR
                            [--log F] [--all] [--key F] [--coverage partial|complete] [--without instructions]
                            [--uploaded N]                                                       # in the Grok box
botport mirror apply-pack --pair P --dir PAIRDIR (--agent-id ID [--agent-data DIR] [--with-skills] | --from EXPORT)
                          --stage DIR [--log F] [--check] [--precheck ITEM --gen N] [--key F]
                          [--without instructions]                                               # in the Grok box
botport mirror recent --pair P --dir PAIRDIR --key F                                             # in the Grok box
botport mirror digest-items|apply-pack|recent --root R [--pair P]... [other flags as above]      # one box, several pairs
```

- **Files.** The pair config is `$XDG_CONFIG_HOME/botport/mirror/<pair>.toml` (0600, outside the profile, never
  mirrored); the mirror state is `<profile>/botport.mirror.json` (digests, names and numbers only); the report of
  conflicts and approvals is `<profile>/botport-mirror-conflicts.md` (rewritten every run, never read back); kept
  copies are under `<profile>/botport/mirror-store/`; one metrics row per sync goes to
  `$XDG_STATE_HOME/botport/metrics.csv`.
- **init** reads both sides; items already equal get bases, every other item (one-sided ones too) goes to the
  pick-one list (`--copy-one-sided` leaves one-sided items to the first sync instead). Pairs are signed: the first
  init makes a pair key file (0600) beside the config and stops until the Grok box has written a state file signed
  with its copy; then run init again. `--key-file F` uses an existing key, `--no-pair-key` makes an unsigned pair,
  which sends nothing to the Grok side and carries no skills.
- **sync** holds the profile's operation lock for the whole run, writes Hermes-bound changes behind a restore point
  (every mirrored category and the mirror state), re-reading each item right
  before it swaps it in, and publishes one cumulative change pack for the Grok side (`HEAD` only moves forward, from
  the predecessor it planned against). No state file, no sync. More than 3 retires on a side (or over 20% of a kind)
  stop the run unless `--allow-retire`. `--trigger F` runs it only for a fresh trigger signed with the pair key
  (5 minutes, nonce used once).
- **Local-only items.** `mirror pick ITEM --local-only` keeps an item on each side as it is from now on: the pair
  config lists it (`local_only`), every run shows it as not mirrored, and it is never a conflict, sent or retired (a
  send still in flight for it is withdrawn: the next run publishes a pack without it). To mirror it again, remove its id
  from `local_only` in the pair config (the command rewrites that file without comments; `mirror init --force` starts
  the list empty). `mirror pick ITEM --forget` clears a conflict over an item neither side has now; it refuses an
  item a side has or holds back, and where a side is read in part (from an export) botport cannot see whether the item
  is gone from it, so the confirmation says so.
- **Approvals.** By default (`--approve all`) every change to a skill (Markdown included), the bot's name or
  instructions, a routine prompt or a connector, in either direction, waits for `mirror approve` (a removal is carried without it, under the
  retire guard and, in an unsigned pair, the unsigned-read rule) (shown, confirmed on a terminal, bound to
  the digest). `--approve runnable` is the opt-in to holding back only new or changed connectors, new skills,
  non-text skill files, enabling a routine or changing an enabled routine's prompt or schedule, and instructions
  drifting over 25% from the last approved text. Without a pair key, every change seen on the Grok side (additions
  included) waits either way. A connector whose url moves to another host (scheme, host or port) waits under every
  policy, and the prompt says so: the headers and auth kept on the Hermes side for the old host are dropped, never
  sent to the new one.
- **check** runs the connection round trip through the pair folder before the first sync (the bot writes
  `probe/bot.txt`; botport writes two files in a round folder `probe/<round>/`; the bot writes both back to `ack-1.txt`;
  botport overwrites one and the bot writes it to `ack-2.txt`; botport never deletes the bot's files): exit 0 passed, 3 waiting for the Grok side, 2 a direction failed (named). `--reset`
  starts over.
- **watch** runs `mirror sync` whenever what it reads changes on this machine (polling times and sizes every
  `--interval`; no model, no network between syncs); `--every` also syncs that often (5 minutes by default, on every
  transport: the timed sync catches what polling cannot see, such as the Drive API backend or an edit that keeps a
  file's size and time); the pair's configuration is read again when its file changes; `--for` stops after that long;
  `--now` syncs at the start
  (otherwise changes made before the watch started wait for the next change). A failing sync is retried with a
  growing pause (up to `--every`, or 5 minutes).
- **pick** is compare-and-swap: refused when either side changed since the conflict was recorded.
- **First setup: the owner's Mirror bot.** No automated step needs a bot's agreement: each one is a file read or the
  owner's own Mirror bot, and no bot is ever asked to export or reveal itself. While nothing has come from the Grok
  side, `mirror init` writes the Mirror bot's brief
  (AGENT-MIRROR.md) and charter (MIRROR-BOT.md) to `$XDG_CONFIG_HOME/botport/mirror/<pair>.brief/` and prints what
  the owner does: create one Mirror bot per account in the app, if there is none yet, and paste the charter
  (`botport mirror setup --print-charter` prints it too) themselves; then ask it, in its chat, to add the pair, with
  the brief and the box archive attached. The Mirror bot reads each bot's setup from the box files.
- **Grok box** (the routine's steps: [AGENT-MIRROR.md](AGENT-MIRROR.md)). Both box commands read the bot's setup from
  the box files with `--agent-id ID` (`--agent-data DIR`, default `/home/box/agent-data`: the name from
  `agents/<ID>/profile.json`, never through a link), or from an export with `--from` (the manual option). The box
  files hold no instructions, routines or connectors, and the box's skills belong to all its bots together (read, from
  `workflows/` without managed and plugin skills, only with `--with-skills`, the owner's per-pair opt-in; under
  `--root`, an empty `.work/<pair>/with-skills` file): the instructions are reported as not mirrored, and skills,
  routines and connectors as kinds not read (`unread` in the state file), which the host carries neither way (a send in
  flight for one is withdrawn and a conflict over one dropped; `init` and `pick` leave them out; `apply-pack` refuses
  them) and `mirror status` names ("not mirrored: routines (Grok side: not in the box files ...)"); in a hybrid pair a
  kind either read cannot see is not carried. The name is read but never written (`by_hand` in the state file: the
  Mirror bot changes no other bot's setup). A box read is always partial.
- **To apply by hand.** What the Grok side cannot take in itself never goes into a pack (a send in flight for one is
  withdrawn): a change bound for a kind it
  reads but never writes (the name), and a Hermes-side change since the last read to a kind it cannot read (the
  first read of each such kind only notes what is there; a local-only item is never listed), go on a per-pair list that `mirror status` and the report show (with
  `mirror show` to see the copy). An entry for the name leaves the list once both sides agree; `mirror applied ITEM`
  takes any entry off. `pick --keep hermes` for such an item is refused.
- **Several pairs.** `mirror init` notes when other pairs are set up on the same machine: pairs served from one box
  share its workspace, and every bot there can read each pair's key file.
- **Recent activity in the box.** `mirror recent` writes each pair's newest Hermes digest that is signed with the pair
  key (checked like every digest) to `<pair>/from-hermes/digest` in the box, and removes that copy when none verifies
  (an unsigned pair gets none; on Unix through directory handles, refusing a link in the last part of each path,
  elsewhere by path; a copy that cannot be removed is reported, exit 2, as possibly stale); the copy is only as trustworthy as the box workspace, which the bots there share. For a
  bot read from the box files, every Grok-bound change waits for approval whatever `--approve` says; a box read being
  partial, the box only ever receives new skills the owner approved. The box-wide
  skill printed by `mirror setup --print-skill` (`mirror-recent`) tells a bot to read only its own pair's file, found
  through its agent id, and to treat it as notes, never instructions. `digest-items` writes the next numbered
  state file (and the content of items that changed), with every kind `partial` unless `--coverage complete` (only
  for a program read of the live setup; given for an export, an item the export left out would read as removed,
  and only the mass-retire guard and the approval rules stand in the way): an item missing from an export is then never taken as removed, and replacing
  or removing an item on the Grok side is left to the owner (add-only, listed with the reason);
  `apply-pack` checks the pack `HEAD` names (refusing one older than a pack it already applied, by the applied log and by
  the floor it keeps beside it, `applied-floor`: the newest generation applied in full, at which nothing is staged
  again; only when the owner confirms a pair was set up again from an empty folder does the box move its applied log
  and floor aside), stages what may be installed, refuses an item changed since the pack was
  made, holds every item of a pack not signed with the pair key and reports it to the owner ("held": an unsigned pack is
  never applied),
  and logs installs it finds done; `--precheck` re-checks one item right before the bot installs it, including (with `--stage DIR`, required)
  that the staged copy is still exactly what the pack wants. A pick or approval that retires an item applies the
  mass-retire guard too (`--allow-retire`); `mirror restore --from` takes at least 12 hex characters matching exactly
  one kept copy, and restores it only if its bytes still hash to its name. With
  `--without instructions` (a bot that does not hand out its live instructions) both commands report
  `persona.instructions` as not mirrored: both sides keep their own copy and nothing is staged over it.
  `digest-items --uploaded N` (N: the newest state file the bot uploaded completely) writes nothing and prints
  `unchanged: state-N.json is current` when the read equals that file and it is the newest; when a newer file was
  never uploaded, the next one is written with every blob. With `--root R` both commands run for every pair with an
  `agent-id` file (the bot's agent id, one line: its setup is read from the box files) or an export under
  `R/.work/<pair>/` (or each `--pair` given): the pair folder copy is `R/<pair>/`, and the export,
  stage, applied log, pair key and upload record are `R/.work/<pair>/export`, `stage/`, `applied.jsonl`, `pair.key`
  and `uploaded` (no `pair.key`: an error for that pair, unless an `unsigned` file marks the pair unsigned); a pair or
  work folder that is a link is refused. Each pair's part of the output starts with a line beginning with its name,
  and a failing pair gets an `error:` line (exit 2) without stopping the others.
- **Transports.** `local` reads the local records directory (`botport-local/1` records per bot key; read only),
  `drive` the pair's Drive folder (the Drive API through `curl` with a token from `--token-cmd`, handed to curl on
  its stdin, never in argv; the command runs for every call, so it should cache the access token in an owner-only
  file until near expiry; one listing call per folder level in a quiet sync, downloads cached in `<profile>/botport/drive-cache/`;
  or the folder Google Drive for desktop syncs), `hybrid` reads kind by kind from local where complete and sends by Drive.
- Exit codes: 0 clean; 1 differences (`plan`, `status`); 2 usage or refused; 3 conflicts or approvals remain.

### Running `mirror watch` as a service
`mirror watch` is meant to run unattended, with no model involved. Run it at login with the system's own service
manager; it needs no network access between syncs except for the Drive API backend's timed check. On macOS, a launchd
agent (`~/Library/LaunchAgents/com.example.botport-mirror.demo-bot.plist`, then `launchctl bootstrap gui/$(id -u)` that
file):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.example.botport-mirror.demo-bot</string>
  <key>ProgramArguments</key><array>
    <string>/usr/local/bin/botport</string><string>mirror</string><string>watch</string>
    <string>--pair</string><string>demo-bot</string><string>--now</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
  </dict>
  <key>StandardOutPath</key><string>HOME/Library/Logs/botport-mirror-demo-bot.log</string>
  <key>StandardErrorPath</key><string>HOME/Library/Logs/botport-mirror-demo-bot.log</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
</dict></plist>
```

launchd expands neither `~` nor `$HOME`: replace `HOME` in the two log paths with your home folder's absolute path.

On Linux, a systemd user unit (`~/.config/systemd/user/botport-mirror-demo-bot.service`, then
`systemctl --user enable --now botport-mirror-demo-bot`):

```ini
[Unit]
Description=botport mirror watch (demo-bot)

[Service]
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=/usr/local/bin/botport mirror watch --pair demo-bot --now
Restart=on-failure

[Install]
WantedBy=default.target
```

One watcher per pair. Each sync takes the profile's operation lock: a manual `mirror sync` that starts while the
watcher is syncing is refused (run it again a moment later), and a watcher sync that meets a manual one retries.
A service starts with a short `PATH`: set it as above so the `hermes` command (and a relative `--token-cmd`) are found,
or give `mirror init --hermes-bin` an absolute path. On macOS, a background agent may be refused access to a synced
Drive folder without any prompt: grant the binary access in the privacy settings, or use the Drive API backend. Logs go to the file named in the plist (macOS) or to `journalctl --user` (Linux): only sync summary lines; item
content is not logged.

## Activity digest (`digest`, mirror mode)
A small Markdown note per side (`botport-digest/1`, design: [design/MIRROR.md](design/MIRROR.md) section 5): four
sections in this order, each with a line budget (Recent activity 12, Open threads 6, Regular routines 10, Key facts
10), every line at most 160 characters, the whole file at most 4,500 bytes (lines are dropped from the end of the
longest section until it fits; the header records the final byte count). Every line is cleaned (control characters,
HTML comments, Markdown headings, links, backticks and code fences removed); a line that looks like a credential is
dropped, not masked. The same inputs give the same bytes, and an unchanged digest is not rewritten. A digest is summary
level only: routine names, schedules, time zones and last-run status, skill and routine counts, session times and
message counts, and session titles only with `--titles`. It never carries the contents of the memory and user-profile
files (memories or user facts; `memories/` is never read), nor approval, form or secret payloads; of a message it
carries only the last message's role and whether it ends in a question mark, never its text. Routine names are shared as
written, so keep secrets out of them. A routine name or session title can still carry text that a model reads as
instructions (a stated residual risk that cleaning and the untrusted-notes framing reduce but cannot remove); `digest
inject --mode pointer` keeps the digest text out of the injected context (a bot that can read files may still open it).

`--hermes-home` here is the profile's own `HERMES_HOME` (the profile dir, as Hermes runs a hook); add `--profile
NAME` to name a profile under a Hermes home instead, or give `--profile-dir DIR`.

- `digest write` builds `<profile>/botport/digest-hermes.md` (file 0600, folder 0700, written atomically) from the
  profile's `state.db` (read with `sqlite3 -readonly -json` and a 200 ms busy timeout; beyond the columns used to filter and
  order sessions, only start/end times, message count (Hermes' own count for the session, or else the rows it still
  shows: active messages and compaction summaries, never inactive rows), the session title (only with `--titles`) and
  the last message's
  role and whether it ends in a question mark, never its text; every session is "session" unless the owner
  passes `--titles`, since titles are usually generated from the conversation; with it, a session without a title is
  "untitled session"), `cron/jobs.json` (routines, and how many are enabled), the skill folders under `skills/` (counted, not
  opened) and, if present, `botport.mirror.json` (only the newest `items.*.at` and the number of `conflicts`). Key
  facts are those setup facts (`3 skills, 1 routine (1 enabled)`, `last mirror change 01-01 09:00, 1 open conflict`);
  `memories/` is never read. Only owner sessions are read: source `cli`, `tui`,
  `desktop` or `local`, or a user id given with `--owner-user` (repeatable); never `cron`. Window: `--window-days`
  (default 7). Without `sqlite3` or `state.db` the session sections are empty and the command still succeeds; when
  the database is busy it exits 0 and writes nothing (the next session end writes). No network, no model call, no
  operation lock. Meant for the `on_session_end` hook.
- `digest show --file F` checks a digest (header, byte count, the four sections in order, budgets, cap) and prints it,
  cleaned again; exit 2 if it is not a valid digest.
- `digest show --file F --pair P --key K` prints a digest copied from the pair folder only when the `F.mac` beside it
  verifies with the pair key (HMAC-SHA256 over the pair, the file name and the bytes); with a key, `mirror sync`
  signs the Hermes digest it puts in the folder and reads a Grok digest from it only when signed. A pair without a
  key reads no digest from the folder.
- `digest inject` is the `pre_llm_call` hook. It reads the hook's JSON from stdin (`is_first_turn`, `platform`,
  `sender_id`, at the top level or under `extra`) and prints `{"context": "..."}` only on the first turn of an owner
  session: `platform` is a local surface (`cli`, `tui`, `desktop`, `local`) or `sender_id` is one of the
  `--owner-user` ids (repeatable). It fails closed: a gateway platform without a listed sender, a missing platform
  without a listed sender, or `cron` (always) prints nothing. The context is
  the peer digest (`<profile>/botport/digest-grok.md`, or `--peer-file`) framed as untrusted notes between
  delimiters, or with `--mode pointer` one line that shows only a path inside the profile or a plain file name (else
  a fixed label), never an absolute path. `<profile>/botport/digest-grok.md` is written by botport only from a digest that verified with the pair key
  (or from the local records); `--peer-file` is trusted local input: a digest file the owner made or chose on
  this machine, read as it is, with no signature check. A missing, invalid or stale peer digest (older than
  `--max-age-days`, default 14, by the older of its `written=` stamp and the file's mtime) prints nothing. It always
  exits 0 and writes nothing. An owner who wants no digest text injected into a model's context uses `--mode pointer` in
  the hook (a bot that can read files may still open the file it names) or leaves the inject hook out.
- **External programs.** `sqlite3`, `curl` and the token command run by absolute path: a bare name is looked up in
  `PATH`, skipping an entry that is itself a link, and an entry unless every folder in it, from the root down, is
  owned by root or the user and writable by no one else (a sticky folder such as /tmp is not trusted; a link on the
  way is followed only if root made it, as on systems where `/bin` points to `/usr/bin`, and its target is checked the
  same way; a Nix store, which its build group can write, is therefore not trusted); the program file itself must be
  owned by root or the user and not writable by others, and one reached through a link must also sit in such a folder,
  and `sqlite3` (3.33 or later) and `curl` (7.52 or later) must be new enough, also when given with a full path. These
  checks are Unix-only: on other systems no external program is trusted. The check and the run are separate steps, so
  a program replaced in between would run (a residual risk on a machine where someone else can write those folders;
  that is what the check refuses).
- **Folder store.** Files in a pair folder on disk are read and removed only through real folders (a link anywhere
  on the way is refused) and only as regular files; checking and removing are separate steps, so a folder that someone
  else could change between the two would be a risk, which is why every folder botport writes is 0700 and the user's.
- **Where digests live and how to delete them.** On the host: `<profile>/botport/digest-hermes.md` and
  `<profile>/botport/digest-grok.md` (0600; rewritten in place, one of each). In the pair folder: `digest/hermes-<n>.md`
  and, with a key, `digest/hermes-<n>.md.mac`; botport keeps the newest 3 and deletes older ones as it writes new
  ones. Of the Grok side's digests, the newest one whose signature verifies is read (checked newest first, up to 64,
  each before any is passed over). A digest holds routine names, schedules, time zones and last-run status and, with
  `--titles`, session titles, which then reach the other platform and are
  kept by the folder's provider (its trash and revision history included) until deleted there. To delete them: remove
  the digest hooks from the profile's `config.yaml`, delete the two host files, delete `digest/` in the pair folder
  and empty the provider's trash. `mirror init --undo` stops syncing but leaves digest files in place.

### Routine parity (`mirror routines`)
`botport mirror routines --bundle EXPORT (--profile NAME [--hermes-home DIR] | --profile-dir DIR) [--tz ZONE] [--json]`
compares the export's routines with the profile's cron jobs by name (the `<Bot>: ` prefix botport gives imported jobs
is ignored) and prints one line per routine, differences first. It reads both sides and writes nothing but the details
file. Statuses (design: [MIRROR.md](design/MIRROR.md) section 9):

| Status | Meaning |
|---|---|
| `same` | schedule, timezone, prompt and paused state match |
| `schedule-differs` | the normalised schedules differ (both shown) |
| `tz-differs` | same schedule, different timezone |
| `prompt-differs` | the prompts differ: 8-digit sha256 digests are shown, never the text |
| `paused-differs` | one side is paused; marked expected when botport paused the job on import, awaiting you |
| `only-grok` / `only-hermes` | missing on one side |
| `event-only` | an event-triggered routine Hermes cron cannot hold: reported, not a failure |
| `duplicate-name` | the name is listed more than once on one side (also once the `<Bot>: ` prefix is removed): not compared, each entry needs a look |

Schedules are normalised before comparing: a five-field cron with whitespace collapsed, day and month names as numbers,
value lists sorted, `7` read as Sunday (`0`); an interval a cron line expresses exactly becomes that line (`every 1d at
05:15` is `15 5 * * *`, `every 6h` (or Hermes' `every 360m`) is `0 */6 * * *`, `every 15m` is `*/15 * * * *`); any
other interval is compared in minutes (`every 90m`); other text as written. Prompts are compared ignoring line endings
and trailing whitespace. Hermes cron has no per-job timezone, so a Hermes job (and any routine that names no zone)
counts as `--tz`, your timezone (default `UTC`). Exit codes: 0 when every routine is `same`, `event-only` or an
expected `paused-differs`; 1 otherwise; 2 on a usage error or an unreadable side.

### Connector mapping (mirror mode, library only)
`src/adapters/connectors.toml` (compiled in) maps public, off-the-shelf connectors between the two sides by normalised
name (case, spaces and punctuation ignored; a description after `(`, `;` or ` - ` is ignored too): GitHub, Notion,
Linear, Slack, Gmail, Google Drive and Google Calendar. Each row names the Grok app, the vendor's official remote MCP
endpoint for the Hermes `mcp_servers` entry (checked against the vendor's own docs, linked in the row; the Google
Workspace endpoints are a Developer Preview) or a manual step, and the credential **names** the Hermes side needs.
Anything else is carried as is: an app with an MCP url keeps its id and url (https only; no userinfo or credential-like
query; a private, loopback or link-local host only if that exact url is already configured on the receiving side), and
an app with neither becomes an opaque record of its `id`, `name`, `kind`, `url` and `tools`, sent back unchanged and
listed as "carried as is". No command uses the table yet; v0.1 behaviour is unchanged.

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


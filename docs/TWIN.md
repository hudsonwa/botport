# Twins: `botport grok list` and `botport twin`

The stable JSON interface a Hermes plugin (such as `grok-bot-twin`) calls. Code: `src/twin.rs`, `src/twin_demo.rs`,
`src/adapters/grok_bot/cache.rs`; tests: `tests/twin.rs`. If this page and the code disagree, the code wins and this
page has a bug.

## What a twin is
A twin is a mirror pair (see [MIRROR-BOT.md](MIRROR-BOT.md)) between one bot in the Grok desktop app and one Hermes
profile, set up and run on the owner's machine.
- **Every twin file lives under `--state-dir`**: the pair config and key, the Grok-side snapshots, the bot's last
  export if one was given, and an index. The profile keeps only its own mirror records (listed under "Files").
- **The Grok side is composed on this machine** from what the Grok desktop app's local cache holds about the bot (its
  id, name, description and routine names, nothing else), laid over the bot's last export if there is one. It is
  written as a signed state file (pair key, HMAC-SHA256) that the mirror engine reads like any other.
- **botport makes no network calls for a twin.** The one route to the Grok side is the owner's Mirror bot, through a
  folder the owner's own sync app keeps in step (`twin mirror`).

## What the cache reader opens
Only regular files (never links) in `<Grok app dir>/sand-client-persistence/` named `<base32 key>.blob`, whose key is
one of these two slices, each at most 16 MiB:
- `sand.client.slice.account.<account>.roster.last-roster`: the bot list (id, name, description; group, hidden and
  removed rows are skipped). When several accounts' rosters are present, the newest wins.
- `sand.client.slice.account.<account>.transcript.replicas.<bot id>`: routine names the app saw created (only for bots
  opened in the app; may be incomplete).

It **never opens** `sand-secrets.json`, cookies, or any other file in that folder or the app folder: their names never
match. A cache folder that is a link is refused before anything is opened. A blob that does not parse is skipped and
counted (`cache_unreadable` warning; the file is never named, since names carry the account). Names are cleaned of
control and direction characters and capped at 200 characters, descriptions at 4000; ids must be 1-128 of
`A-Z a-z 0-9 . _ : -`. Treat them as untrusted text all the same.

Default app dir: the Grok desktop app's own data folder on this computer (the same one Nous's agent-importers
plugin reads). `--grok-app-dir DIR` overrides it. When that folder holds more than one Grok account, botport never
guesses: it lists the accounts by an opaque 8-character label and needs `--account LABEL` once (remembered after).

The cache layout follows the official agent-importers plugin (MIT, Nous Research), which reads the same slices; credit
to that project (see [NOTICE](../NOTICE)).

## What syncs, honestly
botport opens the bot list (id, name, description) and, to find routine names, the selected bots' whole transcript
files (capped: 200 files, 64 MiB; only routine-change events are read from them, nothing else is kept or shown). The
app's local files hold no instructions, skills, schedules or connectors.

| Item | Grok -> Hermes | Hermes -> Grok |
|---|---|---|
| name, description | syncs in seconds, from the cache | by hand (in the Grok app) |
| instructions | by hand, or a one-time export at create | by hand |
| skills | by hand, or a one-time export at create (the box reads back only skills this twin installed) | through the Mirror bot (`twin mirror`); else by hand |
| routines | by hand (names listed for you) | by hand |
| connectors | by hand | by hand |

`twin status` reports the same table as `coverage` (`syncs`, `syncs_via_mirror`, `by_hand`). The description lands in
its own block at the top of the profile's `SOUL.md` (`persona.description`), owned by the Grok side. A twin's skills
live in the `grok-twin` skill category; `grok-bot-imports` (the official importer's) is mirrored too when the profile
has it. Anything the Grok side cannot see is never taken as removed: both the cache read and an export are partial.

## Envelope, exit codes, errors
With `--json`, stdout is exactly one JSON line, errors included; anything for a person goes to stderr. Without it, at
most 10 summary lines.

```json
{"schema":"botport.twin.v1","ok":true,"command":"twin status","data":{},"warnings":[{"code":"..","message":".."}],"error":null}
{"schema":"botport.twin.v1","ok":false,"command":"twin status","data":null,"warnings":[],
 "error":{"code":"pair_not_found","message":"no twin \"nobody\"","hint":"botport twin status --all"}}
```
`error.hint` is a string or `null`.

Exit: `0` ok, `1` ok but the owner's attention is needed (a conflict, a change waiting for approval, a secret held
back, the bot gone from the cache, a failed doctor check), `2` usage, `3` environment, `4` internal.

| Error code | Exit | When |
|---|---|---|
| `usage` | 2 | bad flags; an unknown `--grok-id`; an export that cannot be read |
| `pair_not_found` | 3 | no such twin |
| `profile_not_found` | 3 | `twin adopt` of a profile that does not exist |
| `locked` | 3 | another botport twin command holds the state folder (or the profile); retry, it never waits |
| `grok_app_not_found` | 3 | the cache folder is a link (a missing app is only a warning) |
| `secret_refused` | 3 | the engine refused a write because of a suspected secret |
| `internal` | 4 | anything else (a damaged `twin.json`, an IO error) |

Warnings: `grok_app_not_found` (no cache on this machine: `ok:true`, `app_found:false`, `bots:[]`), `cache_unreadable`,
`no_bots`. Times are UTC (`2000-01-01T09:00:00Z`). No absolute path leaves botport: paths are shown as `state:...`
(under `--state-dir`), `hermes:...` (under `--hermes-home`) or `~`. Suspected secrets are held back and masked
wherever text is shown (a check, not a guarantee).

## Common flags
`--json --hermes-home DIR --state-dir DIR [--grok-app-dir DIR] [--hermes-bin PATH]`. `--hermes-home` defaults to
`$HERMES_HOME`, then `~/.hermes`; `--state-dir` to `<hermes home>/botport-twin` (the plugin passes its own folder).
`--only GLOB` (repeatable) filters by bot name, case-insensitive, `*` and `?`.

## Commands
Examples below come from `botport twin demo`, trimmed (`…`); times changed to 2000.

### `botport version --json`
```json
{"version":"0.2.0","schema":"botport.twin.v1","features":["grok-list","twin-create","twin-check","twin-sync","twin-status",
 "twin-conflicts","twin-resolve","twin-approve","twin-restore","twin-recent","twin-remove","twin-mirror","twin-parity","twin-doctor","twin-demo"]}
```
A feature is listed only when it is implemented; a plugin should check it.

### `botport grok list [--only GLOB]...`
```json
{"app_found":true,"bots":[{"grok_id":"b-acme-0001","name":"Acme Helper","description":"Answers questions about Acme widgets.",
 "routines":["Morning summary"],"twinned_pair_id":"acme-helper",
 "fields":{"name":"live","description":"live","instructions":"mirror","skills":"mirror","routines":"names"}}, …]}
```

### `botport twin create (--grok-id ID [--profile NAME] | --all [--only GLOB]...) [--from-export FILE] [--approve runnable|all] [--dry-run]`
Creates the profile (`hermes:profiles/<pair>`) when missing, the pair key, the pair, and a first Grok-side snapshot,
then syncs. The pair id comes from the bot's name (`acme-helper`). For a new profile, creating it after the preview
is the owner's approval of what the Grok side holds now. An existing profile is adopted: nothing is overwritten and the
differences are listed. `--from-export` (one bot) is a `grok-bot-export` file; only its screened items are kept
(`state:sources/<pair>/items/`, never the file itself) so later syncs lay the cache over them again. `--approve` defaults
to `runnable`; a new twin's first copy of its name, description, instructions and skills is approved by the create,
routines and connectors still wait. `--dry-run` writes nothing and shows a `preview` of each item, with lines that look
like secrets masked first; an item held back as a suspected secret is not in `items`, only in `secrets_withheld`.
```json
{"dry_run":false,"skipped":[],"pairs":[{"pair_id":"acme-helper","state":"in_step",
 "grok":{"id":"b-acme-0001","name":"Acme Helper"},"hermes":{"profile":"acme-helper","created":true,"adopted":false},
 "items":[{"item":"persona.name","kind":"name","name":"name","action":"create","owner":"grok","preview":"Acme Helper"},
  {"item":"skill:greet-visitors","kind":"skill","name":"greet-visitors","action":"create","owner":"grok","preview":"--- SKILL.md\n…"},
  {"item":"routine:Morning summary","kind":"routine","name":"Morning summary","action":"by_hand","owner":"grok"}, …],
 "applied":[{"item":"skill:greet-visitors","kind":"skill","name":"greet-visitors","direction":"g2h"}, …],
 "waiting":[],"conflicts":[],"secrets_withheld":[],"to_grok":0,"schedules_paused":0,"restore_point":null,"wall_ms":113}]}
```
`skipped` reasons: `already_twinned`, `profile_already_paired`.

`botport twin adopt --grok-id ID --profile NAME [--from-export FILE]` is the same for a profile that must already exist
(for example one made by `hermes import-agent grok-bot`).

### `botport twin check`
Stat only (time, size, inode of the cache slices, the profile's setup files and the export); opens no content, takes
no lock. A first run counts every twin as changed. A command that wrote settles the signatures, so its own writes do
not count.
```json
{"changed":["acme-helper"],"checked_at":"2000-01-01T09:00:00Z"}
```

### `botport twin sync [--pair ID | --all] [--only GLOB]... [--dry-run]`
Refreshes each Grok-side snapshot from the cache (a new signed state file only when something changed), then runs the
mirror engine: ownership merge, approvals, conflicts. `--dry-run` plans only. `twin pack` is the same command.
```json
{"pairs":[{"pair_id":"acme-helper","state":"needs_you","dry_run":false,
 "applied":[{"item":"persona.name","kind":"name","name":"name","direction":"g2h"}],
 "waiting":[],"conflicts":[],"to_grok":0,"restore_point":null,"wall_ms":160,
 "secrets_withheld":[{"item":"persona.description","kind":"description","direction":"g2h",
  "location":"suspected secret in the description: token-shaped value (sk-/xai-/…) (a check, not a guarantee)"}]}, …]}
```
`state`: `in_step`; `changes_waiting` (something bound for Grok); `needs_you` (a conflict, an approval, or a secret held
back); `grok_missing` (the bot is gone from the cache). `waiting` entries carry `reason: "needs_approval"` and `detail`.

### `botport twin status [--pair ID | --all] [--only GLOB]...`
```json
{"pairs":[{"pair_id":"acme-helper","state":"changes_waiting","grok":{"id":"b-acme-0001","name":"Acme Helper","present":true},
 "hermes":{"profile":"acme-helper"},"last_sync_at":"2000-01-01T09:00:00Z","last_change_at":"2000-01-01T09:00:00Z",
 "pending":{"to_hermes":0,"to_grok":1,"in_flight":0,"waiting_approval":0},"conflicts":0,
 "coverage":[{"item":"name","g2h":"syncs","h2g":"by_hand"},{"item":"skills","g2h":"by_hand","h2g":"by_hand"}, …],
 "by_hand":[{"item":"routine:Morning summary","side":"hermes","what":"Recreate routine \"Morning summary\" in Hermes (Grok keeps its schedule in the cloud)"},
  {"item":"skill:greet-visitors","side":"grok","what":"Apply the Hermes copy of skill \"greet-visitors\" in the Grok app"}],
 "mirror_bot":{"connected":false},"restore_points":7,"secrets_withheld_total":0,"secrets_withheld":[],"error":null}]}
```
`in_flight`: sent to the Mirror bot, not yet installed by it. `state` may also be `error` (with `error` set).

### `botport twin conflicts --pair ID`
Both copies of each open conflict and a line diff (`-` Grok, `+` Hermes). Each side's text is capped at 64 KiB
(`text_truncated`); lines that look like secrets are replaced by `[line hidden by botport: suspected secret]`. Exit 1
while any conflict is open.
```json
{"conflicts":[{"conflict_id":"skill:greet-visitors","kind":"skill","name":"greet-visitors","since":"2000-01-01T09:00:00Z",
 "grok":{"digest":"ea8d70adba66","text":"…Welcome the visitor by name and mention this week's widget offer.\n","text_truncated":false},
 "hermes":{"digest":"aa44e30d6551","text":"…Always end with: \"Anything else I can help with?\"\n","text_truncated":false},
 "diff":"--- grok\n+++ hermes\n …\n+Always end with: \"Anything else I can help with?\"\n-Welcome the visitor by name and mention this week's widget offer.\n"}]}
```

### `botport twin resolve --pair ID --conflict ITEM --keep hermes|grok`
```json
{"applied":[{"item":"skill:greet-visitors","keep":"hermes"}],"restore_point":null}
```
Keeping the Hermes copy of a skill puts it on the Grok side's list (`to_grok`): by hand, or through the Mirror bot.

### `botport twin approve --pair ID --item ITEM`
Approves one change listed under `waiting`, then syncs that twin. Returns `{"approved":[{"item":..,"to":..}],"sync":{…}}`.

### Restore points: `twin restore-points --pair ID`, `twin restore --pair ID --point ITEM@DIGEST [--dry-run]`
Before the mirror engine replaces an item in the profile (a sync, or `twin resolve --keep grok`), it keeps the Hermes
copy it replaces in the profile's mirror store (`botport/mirror-store/`), named by its sha256, as well as each copy it
writes, and writes a zip of what it changes to
`backups/botport/restore-<time>-<n>.zip`. `restore-points` lists the kept copies, newest first; `restore` puts one back
(the digest part needs at least 12 hex characters; the copy must still hash to its name).
```json
{"points":[{"id":"persona.name@8ef22c6d10ba","item":"persona.name","kind":"name","name":"name",
 "at":"2000-01-01T09:00:00Z","reason":"kept copy","scope":"item"}, …]}
{"restored":["persona.instructions"],"dry_run":false,"restore_point":null}
```

### `botport twin recent --pair ID [--limit 10]`
Metadata only, no content; the last 50 are kept. `side`: `grok`, `hermes`, `both`. `kind`: `sync`, `skill_change`,
`name_change`, `routine_change`.
```json
{"entries":[{"at":"2000-01-01T09:00:00Z","side":"grok","kind":"name_change","summary":"the bot was renamed in Grok"}, …]}
```

### `botport twin remove --pair ID [--dry-run]`
Stops twinning: removes the pair config and key, its snapshots, its kept export, and botport's files in the profile
(`botport.mirror.json`, `botport-mirror-conflicts.md`, `botport-manual.md`, `botport/`, `backups/botport/`, so the kept
copies and restore zips go too). The profile stays as a normal Hermes profile.
```json
{"pair_id":"recipe-planner","dry_run":true,"profile_kept":true,
 "removed":["state:pairs/recipe-planner.toml","state:pairs/recipe-planner.key","state:grok/recipe-planner", …,"hermes:profiles/recipe-planner/backups/botport"]}
```

### `botport twin mirror --pair ID (--pair-folder DIR | --off)`
Connects the twin to the owner's Mirror bot, so skills travel both ways. botport only
reads and writes files in the pair folder on this machine; the owner's sync app (for example Google Drive for desktop)
keeps it in step. It makes no network calls and holds no Drive token. The folder must exist, be a real folder (not a
link) and sit outside the Hermes home and the state folder. The output lists the steps, the briefs written for the
Mirror bot (`state:pairs/<pair>.brief/`), the pair key's location and the charter text:
1. If the Grok account has no Mirror bot yet, create one in the Grok app and paste the charter as its instructions.
2. Share the pair folder with the Mirror bot's Google Drive connection.
3. In the Mirror bot's chat, attach `AGENT-MIRROR.md` and the pair key, and ask it to add the pair for the bot (by its
   agent id); because it is a twin, it uses `pair-skills`, so only the skills this pair installed come back, never
   another bot's.
4. Press **Test** on the Mirror bot's routine, then sync.
```json
{"pair_id":"acme-helper","connected":true,"briefs":"state:pairs/acme-helper.brief","pair_key":"state:pairs/acme-helper.key",
 "steps":[{"id":"mirror_bot","what":"…"},{"id":"share_folder","what":"…"},{"id":"add_pair","what":"…"},{"id":"test","what":"…"}],
 "charter":"# botport — Mirror bot charter\n…"}
```
`--off` disconnects it (`{"pair_id":"acme-helper","connected":false}`); skills go back on the by-hand list.

### `botport twin parity --pair ID`
Setup parity, as far as botport can see: one check per mirrored item (passes when the same on both sides), one per
routine the cache names (passes when a Hermes cron job with that name exists), and one for a model set on the twin. No
model is called; for a behaviour test use `botport test`. Exit 1 when a check fails.
```json
{"kind":"setup","passed":5,"total":7,"checks":[{"id":"skill:greet-visitors","name":"skill greet-visitors","pass":true,"detail":"the same on both sides"},
 {"id":"routine:Morning summary","name":"routine Morning summary","pass":false,"detail":"not recreated in Hermes yet (Grok keeps routine schedules in the cloud)"},
 {"id":"model","name":"model","pass":false,"detail":"the twin has no model set: hermes -p <profile> model"}, …]}
```

### `botport twin doctor`
Checks `botport_version`, `grok_app`, `hermes_cli`, `state_dir`, and `twin-<n>` for each twin; exit 1 when one fails.
```json
{"checks":[{"id":"grok_app","ok":true,"message":"the Grok desktop app's cache is readable","fix":""}, …]}
```

### `botport twin demo [--step rename|secret|conflict|reset]`
Invented bots (Acme Helper, Recipe Planner) in a fake Grok app folder (`state:demo-grok-app`), with an export each
(`state:demo/exports/`), then `twin create --all`. No Grok account is needed, and the steps change only that fake
folder. Give the demo its own folders; it refuses a state folder that already holds real twins.
```sh
d="$(mktemp -d)"; c="--json --hermes-home $d/home --state-dir $d/state"
botport twin demo $c                     # two twins, in_step
botport twin demo --step rename $c       # Acme renamed in the fake cache; the twin's SOUL.md follows
botport twin demo --step secret $c       # a fake key in the description: held back, exit 1
botport twin demo --step reset $c        # back to the original cache
botport twin demo --step conflict $c     # one skill changed on both sides: a conflict, exit 1
botport twin conflicts --pair acme-helper $c
botport twin resolve --pair acme-helper --conflict skill:greet-visitors --keep hermes $c
```
Each step then syncs and answers like `twin sync`.

## Suspected secrets
An item that looks like it holds a secret is **held back whole**, in both directions: never copied, never redacted and
copied. The output says which item and where (`location`: the kind, a line, the shape), never the value. Detection
covers known key formats (including `sk-`, `xai-`, `AKIA`, `AIza`, `sk_live_`/`rk_live_`, `hf_`, `npm_` and private
key blocks) and, for the bot's name, description and instructions (they land in the twin's system prompt), any word of
32 or more characters that looks random: long hex, or letters and digits mixed with high entropy. It is a check, not a
guarantee: keep secrets out of a bot's text.

## Files
Under `--state-dir`: `twin.json` (index), `check.json` (stat signatures), `metrics.csv`, `pairs/<pair>.toml`,
`pairs/<pair>.key` (0600), `pairs/<pair>.brief/`, `grok/<pair>/state-<n>.json` and `blobs/`,
`sources/<pair>/export.json`, and for the demo `demo-grok-app/` and `demo/exports/`. In the profile:
`botport.mirror.json`, `botport-mirror-conflicts.md`, `botport-manual.md`, `botport/`, `backups/botport/`. Writes take
a lock on the state folder (`.botport.lock`) and fail fast with `locked` when another run holds it.

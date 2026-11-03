# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- `mirror recent` removes a stale copy through directory handles that refuse a link in the last part of each path (on
  Unix; elsewhere by path), and reports a copy it cannot remove. The digest pointer resolves the path before showing it
  and shows only a path inside the profile or a plain file name (else a fixed label). `mirror init` notes
  when other pairs are set up on the same machine (they may share a box workspace and its key files).
- For a bot read from the box files, every change bound for it waits for the owner's approval whatever the policy, so
  the Mirror bot only ever installs new skills the owner approved. `mirror recent` removes a pair's copy when no digest
  verifies any more. The recent-activity skill finds a bot's pair by its own agent id only. The docs say that bots
  sharing a box workspace can read each other's pair key files there.
- A connector url with userinfo, a backslash, percent-encoding or a non-ASCII host has no origin: it never counts as
  the same host, so credentials kept for another url are dropped and the change waits for approval. `digest inject
  --mode pointer` shows only a path inside the profile or a plain file name (else a fixed label), never an absolute
  path.
- The box's previous state file is the highest-numbered one that verifies, whatever order the folder lists them in.
  The host reads the newest Grok digest whose signature verifies, checking each newest first before passing it over.
  The box brief quotes every name in its commands and says which names botport accepts.
- The box keeps the newest pack generation applied in full beside its applied log (`applied-floor`) and refuses an
  older pack by it; at that generation nothing is staged again. `mirror restore` restores a kept copy only if it still
  hashes to its name. A connector whose url moves to another host waits for approval under every policy, and the
  headers and auth kept for the old host are dropped, never sent to the new one. Pair names read from files nothing
  has verified are shown only when valid, else escaped.
- The box keeps its applied log per pair in the single-pair layout too (`.work/<pair>/applied.jsonl`); the refusal of
  an older pack says what to do when the owner set a pair up again, and so does the brief. The host refuses a pack
  generation past the largest number instead of wrapping.
- Packs and digests are chosen only among files the pair key verifies (the newest valid digest counts; file numbers
  are bounded), and the box refuses a pack older than one it already applied. The program-trust checks are Unix-only;
  elsewhere no external program is trusted.
- Under the default approval policy the bot's name waits for approval like the instructions. A pick or approval that
  retires an item applies the mass-retire guard. The box's precheck re-checks a staged skill against the pack, and
  the box reads its previous state file only when the pair key verifies it; the host takes in reported installs only
  from authenticated reads; a local-only pair keeps its replay floor; `mirror restore` needs an unambiguous 12-hex
  prefix.
- The Drive API endpoint cannot be changed outside the module's own tests, and a plain-http test stub must be exactly
  a local address (no userinfo).
- External tools: every folder of a PATH entry is checked without following links, a sticky world-writable folder is
  not trusted, and a program reached through a link must also sit in a trusted folder. A routine's zone name is shown
  in messages only when it uses the tz database's characters.
- External tools: PATH entries that are links are skipped, every folder above a program folder must be owned by root
  or the user and not writable by others, and the version check also applies to a configured
  full path to `curl` or `sqlite3`. The export brief checks the archive with `sha256sum -c -`, writing no file.
- A Hermes whose `cron create` has no `--timezone` gets UTC routines without the option and a clear refusal for any
  other zone; a failed `hermes` call names the first line of its error output (botport's own values masked, a line
  that looks like a secret left out). The metrics file is made 0600 if an older one was not, is never opened through
  a link, and no cell can read as a spreadsheet formula.
- External tools (`sqlite3`, `curl`) are taken only from absolute `PATH` entries owned by root or the user and
  writable by no one else, and must be at least `sqlite3` 3.33 and `curl` 7.52. `digest write --help` warns that session
  titles reach the other platform; the reference says where digest files live and how to delete them.
- Mirror pairs hold every change to a skill, the instructions, a routine prompt or a connector for the owner's approval
  by default (`mirror init --approve all`); `--approve runnable` is the opt-in to carrying Markdown-only skill edits
  and small instruction edits without asking. `digest write` leaves session titles out unless given `--titles`;
  `--no-titles` is still accepted. A connector's host is resolved right before botport writes it, and refused when
  it resolves to a loopback, private or link-local address not already configured there. Skill categories are
  checked like item names.
- Digest files in the pair folder are signed with the pair key (`<name>.mac`); a Grok digest is read from the folder
  only when signed, and `digest show --key` shows a copied digest only when signed. A pair without a key reads no
  digest from the folder. `mirror show` and approval prompts escape terminal control characters and list binary
  package files by size and sha256. `sqlite3`, `curl` and token commands run by absolute path from a checked `PATH`
  (absolute entries that nobody else can write to); `sqlite3` 3.33 or later and `curl` 7.52 or later.
- Mirror state files and change packs carry `neutral_form` (now 2: a `SKILL.md` is compared without ever dropping
  text). A side refuses a file made with another form, so upgrade botport on the host and in the Grok box together;
  the box's next state file continues the numbering and carries every blob again.

### Added
- Chatting with a twin: `mirror status`, `mirror init` and `mirror sync` start with a loud line when the Hermes twin
  cannot be chatted with yet (no gateway, parked, not served, no model) and say the fix; `mirror doctor --pair P`
  checks the gateway and the model. botport reads Hermes' own status files and never starts a gateway. A profile that
  lands without a model gets the default profile's model settings, keeping only the default/model/name/provider fields
  and dropping values that look like credentials. The user guide's new "Chatting with
  your mirrored bots" covers `hermes gateway migrate --multiplex`, its risks and a safe order.
- The Mirror bot changes no other bot's setup: what the Grok side reads but never writes (a bot's name), and Hermes-side
  changes to kinds it cannot read, go on a per-pair list to apply in the Grok app by hand (`mirror status`, the report;
  `mirror applied ITEM`). The box's shared skills are mirrored only for a pair the owner opts in (`--with-skills`).
  `mirror recent` writes each pair's newest signed Hermes digest to `<pair>/from-hermes/digest` in the box, and
  `mirror setup --print-skill` prints the box-wide `mirror-recent` skill that lets a bot read its own pair's summary.
- Onboarding through the owner's Mirror bot: `mirror setup --print-charter` prints the charter the owner pastes into
  the one Mirror bot they create per account; `digest-items` and `apply-pack` read a bot's setup from the box files
  (`--agent-id`, `--agent-data`; under `--root`, `.work/<pair>/agent-id`), never asking the bot: the name (the box's skills are
  shared by all its bots, so none is read as one bot's own); kinds a box read cannot see are recorded as not read,
  carried neither way, and named by `mirror status`. `mirror init` hands the owner
  the Mirror bot's brief and charter; no botport text asks a bot to export itself (AGENT-EXPORT.md is a manual option
  the owner runs in a bot's own chat, and its step for other bots no longer messages them).
- `mirror pick ITEM --local-only` keeps an item on each side as it is and never mirrors it (listed in the pair config,
  shown as not mirrored, never a conflict; a send in flight for it is withdrawn); `mirror pick ITEM --forget` clears a
  conflict over an item neither side has (confirming, where a side is read in part, that botport cannot see it).
- `docs/AGENT-MIRROR.md`: the brief for the Grok side's mirror routine (cheap HEAD check, stage with `apply-pack`,
  install by copying staged files after a per-item precheck, record with `digest-items`, upload the state file).
- `core::mirror`: mirror mode's merge engine (`botport-mirror/1` state). Each item is compared with a base per side;
  a change on one side is carried to the other, a change on both sides is a conflict that stays open until picked or
  made equal. Items missing from an incomplete read are never deleted; more than 3 retires on a side, or more than 20% of a
  kind with at least 5 items, stop the run; changes
  can be held for an approval bound to their digest. Design: [docs/design/MIRROR.md](docs/design/MIRROR.md) section 4.
- Mirror transports: the `Transport` interface, numbered self-hashed state files with content-addressed blobs, a
  folder store, and the local reader interface (`botport-local/1` records per bot key, written by any program that
  produces them). An optional pair key (HMAC-SHA256) signs state files and authenticates host
  triggers (5-minute window, nonce used once). Design sections 3 and 6.
- Drive transport for mirror mode: cumulative change packs named by a forward-only `HEAD`, content-addressed blobs,
  rotating digests; the synced-folder backend, or the Drive API through `curl` (token from a command, passed in a
  `0600` config file, never in argv). Design section 6.3.
- `botport mirror stats`: count and p50/p95 time and payload per command and transport, from a metrics CSV (one row
  per mirror run, `0600`, never leaves the machine). Design section 8.
- `botport mirror init|plan|sync|status|show|pick|approve|restore` and, in the Grok box, `mirror digest-items` and
  `mirror apply-pack`: equal-peer sync of a bot's setup between Grok and Hermes, with a pick-one list for items
  changed on both sides, approvals for changes that can make a bot run something new, restore points before Hermes
  writes and kept copies of what a change replaces on the Grok side, and one metrics row per sync. v0.1 commands are unchanged. See docs/REFERENCE.md "Mirror mode".
- Mirror sync uses the routine-parity normalisation for routine digests, maps app-only connectors through the public
  table, checks a received digest before storing it, and builds the Grok side's digest from local records on the owner's
  machine when there are any.
- `core::package`: opaque skill/tool packages (`botport-package/1`) for mirror mode. A package is carried byte for
  byte with its executable bits; its digest covers every path, byte and executable bit; a generated manifest lets a
  receiver check it. Symlinks, special files, setuid/setgid/sticky bits, unsafe paths, paths that collide by case or
  Unicode normalisation, and packages over the size limits are refused. Writes go to a temp dir, are read back and
  checked, then swapped in (files `0600`, executables `0700`, in a `0700` folder); a crash in the middle of the
  swap is recovered by the next write or read. Design: [docs/design/MIRROR.md](docs/design/MIRROR.md) section 11.
- `botport digest write|show|inject`: the activity digest for mirror mode (`botport-digest/1`, design section 5). `write`
  builds `<profile>/botport/digest-hermes.md` (0600) from the profile's owner sessions in `state.db` (read-only through the
  `sqlite3` command; it emits times, message counts and only the last message's role and whether it ends in a question
  mark, never its text; titles only with `--titles`), `cron/jobs.json`, skill counts and the mirror state's times and conflict
  count, capped at 4,500 bytes, cleaned, credential lines dropped, and not rewritten when unchanged. Summary level only:
  it emits routine names, schedules, time zones and last-run status, skill and routine counts, session times and message
  counts, and session titles only with `--titles`; it never emits the contents of the memory and user-profile files
  (memories or user facts; `memories/` is never read), and of a message only the last message's role and whether it ends
  in a question mark, never its text. Routine names are shared as written, so keep secrets out of them.
  `inject` is a `pre_llm_call` hook that adds the peer's digest as untrusted notes on an
  owner session's first turn only (a local surface, or a `sender_id` listed with `--owner-user`; anything else, and
  cron always, gets nothing), or a one-line pointer with `--mode pointer`. A routine name or session title can still
  carry text that a model reads as instructions (a stated residual risk that cleaning and the untrusted-notes framing
  reduce but cannot remove); `digest inject --mode pointer` keeps the digest text out of the injected context (a bot
  that can read files may still open it). v0.1
  commands are unchanged.
- `botport mirror routines`: a read-only routine-parity check between a Grok export and a Hermes profile, one line per
  routine (`same`, `schedule-differs`, `tz-differs`, `prompt-differs` with short digests, `paused-differs`,
  `only-grok`/`only-hermes`, `event-only`, `duplicate-name` for a name listed more than once on one side, never compared
  by its first entry); exit 1 on a difference. Schedules are normalised first (cron spellings,
  day/month names, Sunday as 0 or 7, intervals a cron line expresses). The normalisation and the routine's neutral form
  live in the platform-free core (`core::routines`) for mirror mode to hash (docs/design/MIRROR.md section 9).
- Connector mapping for mirror mode (library only, no CLI change): a compiled-in table of public connectors (GitHub,
  Notion, Linear, Slack, Gmail, Google Drive, Google Calendar) with their official remote MCP endpoints and the
  credential names the Hermes side needs; name matching ignores case, spaces and punctuation. Other connectors are
  carried as is: an MCP url under a url policy (https only, no credentials in the url, private hosts only when already
  configured on the receiving side), otherwise an opaque record of allowlisted fields. Includes the reverse direction
  and a neutral form for hashing (docs/design/MIRROR.md section 10).

## [0.1.2] - 2026-11-03

### Security
- `export` (and `export-hermes`) no longer follow a symlink when replacing an old export in `--out`: only regular files
  listed in the old manifest, reached through real folders inside `--out`, are removed; anything else is left in place
  and named in the error. Before, a tampered old export listing `link/victim` with `link` pointing outside could delete
  any file the user could write.

### Changed
- Chat history is **opt-in**: `export` leaves it out unless `--with-history` is given (`--history-db` and
  `--history-sqlite` now need it). Nothing on the Hermes side reads history, so import, sync and drift are unchanged.
- Bundles are private: every file `export`/`export-hermes` writes is `0600` and its folders `0700` (were `0644`/`0755`);
  `describe --out` writes `0600`.
- Docs: the user guide no longer suggests keeping an unpacked bundle in git; it says what personal data a bundle holds.

## [0.1.1] - 2026-11-03

### Fixed
- `tests/e2e-real/run.sh` refuses to start if the throwaway profile name already exists, and its cleanup deletes only a
  profile the run itself created (a failed create no longer deletes a same-named profile).
- `g2h uninstall` treats its manifest as a hint: it removes only the exact files setup writes (recomputed under the g2h
  home and the service dir), never symlinks or directories, and reports anything else in the manifest as skipped. A
  unit file (for the manifest's or the config's label) is removed, and its service stopped, only if it is a g2h unit for
  this install; another app's unit is left alone and its service is not stopped.
- The `shim/` integration test says loudly that it skips without the agent-importers source (CI has none; the two unit
  tests still run); docs no longer imply CI runs it. `SHIM_REQUIRE_SRC=1` makes a missing source fail.
- Event schedules survive a Grok round trip (#14): `describe` reads a schedule line whose time is "on an event"
  (or "event-triggered") as `kind: event` with no cron, instead of a schedule Hermes cannot accept. Events are still
  never turned into cron jobs. The NOT_CAPTURED.md botport writes now keeps an event's prompt.

### Changed
- A bundle directory's `profile.json` `instructions` (hand exports; real Grok-box profiles carry none today) now
  ports to the persona, and so to SOUL.md, the same as a grok-bot-export file's instructions (#15). Such a bundle
  gets a new botport-owned persona section appended to SOUL.md on the next apply. Your own text is kept; if you had
  pasted those instructions in by hand, remove your copy.
- Writing a Grok description: an event schedule stays "on an event" and reads back as an event (#14). A timed
  schedule with no cron is now written as "no schedule given", not "on an event", and reads back as a manual item.
- Commands that write into a Hermes profile hold a per-profile OS file lock (`flock` on `<profile>/.botport.lock`):
  `apply`, `sync` (one lock across both halves of `--direction both`), and the `botport.lock.json` record written by
  `export-hermes` and `sync --direction h2g`. A second such run is refused at once with the holder's pid; the lock is
  released by the kernel when the holder exits, so there is no stale-lock logic, and the file is kept. `plan`,
  `drift` and `--dry-run` never take it. It is advisory and per host: `hermes` itself and other machines are not
  covered (docs/SECURITY.md).
- A malformed or unreadable `cron/jobs.json` is now an error for `plan`, `apply`, `sync`, `drift` and
  `export-hermes`, not "no jobs". If it turns up malformed after files were written, the error says so and names
  the restore point.
- If the `--details` report can't be written, one warning on stderr names the path and the reason.

## [0.1.0] - 2026-11-03
First public release. The tool was called grok2hermes before release; the old names still work (see
[REFERENCE.md](docs/REFERENCE.md#compatibility-with-the-old-name)).

### Added
- `--check-routines PATH|auto` (experimental, opt-in): warn about routines the Grok desktop app's cache knows
  that the export left out. This reads your Grok desktop app's local conversation transcripts (the
  transcript.replicas files) in full, on this machine only. It keeps only the names of routines from automation
  events. Nothing else is stored, copied or printed, and nothing is sent anywhere ([REFERENCE.md](docs/REFERENCE.md)).
- `botport` binary: `export`, `verify`, `describe`, `plan`, `apply`, `sync`, `drift`, `export-hermes`, `test`
  and `battery init`. `grok2hermes` stays as an alias.
- Grok Bot adapter: botport bundles and grok-bot-export v1 files.
- Hermes adapter with owned sections, conflict detection, restore points and two-way sync.
- Neutral bot model `bot-model/1` with adapter conformance tests.
- `g2h` and `g2hd`: a local talk bridge between a Grok Bot and a Hermes bot, with setup, selftest and an owner
  approval step for actions.
- Agent briefs for setup, export and import; a worked example porting a public template.
- The `BotModel` seam: `plan`, `apply`, `sync` and `drift` read the source through an adapter (`Adapter`,
  `Source`) and plan and apply it through the Hermes `Target`, so a new platform plugs in as an adapter. The model
  gained `manual[]`, schedules, connectors and credential names, and the conformance suite checks that an adapter
  keeps them. botport also builds as a library.
- `g2hd` separate-user mode (`[separate_user]`, opt-in, not yet run in production): g2hd runs as its own OS user,
  checks peer uids, accepts `approve` only from its own uid or root, and refuses to start on loose home, config or
  approver paths. Recipe: docs/G2H-SEPARATE-USER.md.

### Changed
- Docs (round 2 before release): lead with what works today, an honest roadmap naming the next platforms
  (OpenClaw, OpenBot), the g2h scope stated plainly (it reduces the chance a bot follows peer text as
  instructions; it is not a security boundary), g2h presented as an optional companion, and its known limits
  (per-conversation turn guard, token cap counted after replies, unpruned session map).
- `g2h`/`g2hd` refuse a credentials file not owned by the uid they run as (or root), in same-user mode too.

### Fixed
- Release workflow: a tag with a suffix (`v0.1.0-rc1`) is published as a pre-release, so "latest" keeps pointing
  at the last real release. The release workflow now runs the full CI suite before it builds or publishes.

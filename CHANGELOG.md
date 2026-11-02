# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

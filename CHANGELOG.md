# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - unreleased
First public release. The tool was called grok2hermes before release; the old names still work (see
[REFERENCE.md](docs/REFERENCE.md#compatibility-with-the-old-name)).

### Added
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

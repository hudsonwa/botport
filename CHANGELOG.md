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

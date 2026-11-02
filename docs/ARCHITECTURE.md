# Architecture

Hub and spoke: a platform-free **core** around the neutral bot model (`bot-model/1`, see [MODEL.md](MODEL.md)),
with one **adapter** per platform. Adding a platform means adding an adapter; the core does not change.

```
            adapters/grok_bot  ──read──▶ ┌────────────────────┐ ◀──read──  adapters/hermes
  (export bundle, verify, legacy dirs)   │ core               │   (profile, spec, plan/apply,
                                ◀─write─ │  model::BotModel   │ ─write─▶   sync, twoway, drift)
                                         │  adapter::Adapter  │
                                         │  util, yaml11      │
                                         └────────────────────┘
```

## Today vs target
The Grok Bot and Hermes adapters exist and pass conformance, and `plan`/`apply`/`sync`/`drift` run through the
seam: the Grok Bot adapter reads the bundle into a `BotModel` plus a `Source`, and the Hermes adapter's `Target`
plans and applies that model. Hermes sync never sees a Grok bundle. What is not done: the CLI takes only a Grok Bot
source (`--bundle`), so a new source adapter also needs a way to be selected there; and Hermes is the only `Target`.

| Item | Status |
|---|---|
| Grok Bot ↔ Hermes export/import, sync and drift | done |
| g2h talk bridge (`g2h` + `g2hd`, running as your user) | done |
| Neutral `BotModel` + `Adapter` seam routing plan/apply/sync/drift | done (v0.1.0) |
| OpenClaw (formerly Moltbot/Clawdbot) adapter | planned |
| OpenBot adapter | planned |
| `g2hd` running as a separate OS user | planned |

## Library and CLI
`src/lib.rs` is the library (crate `botport`): the public modules `core`, `adapters`, `battery` and `doctor`.
`src/main.rs` is the CLI on top of it (`use botport::...`); `grok2hermes` is the same CLI under its old name.

## The seam (`src/core/adapter.rs`)

```rust
pub trait Adapter {
    fn id(&self) -> &'static str;                                  // also its key in BotModel::extras
    fn read(&self, src: &Path) -> Res<BotModel>;                    // native layout -> model
    fn read_source(&self, src: &Path) -> Res<(BotModel, Source)>;  // read + what was read (default: no files)
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>>; // model -> native layout
}

pub trait Target: Adapter {                    // an adapter that keeps a live install in step with a model
    type Opts;
    fn plan(&self, m: &BotModel, src: &Source, dest: &Path, o: &Self::Opts) -> Res<Plan>;
    fn apply(&self, m: &BotModel, src: &Source, dest: &Path, o: &Self::Opts, adopt: bool, dry: bool)
        -> Res<Applied>;
}
```

`Source` is sync bookkeeping, not bot content: the source path, its id, a sha256 per file read (the sync digest)
and the skills a spec allowlist left out (`excluded`, filled by `retain_skills`). A `plan`/`apply` run is: source `read_source` -> model
-> `retain_skills` -> `Target::plan` or `Target::apply`. The two-way path (`twoway`, `export-hermes`) uses the
same model and `Source`.

Implementations: `adapters::grok_bot::GrokBot` (id `grok-bot`; reads an export bundle or legacy hand export,
writes the legacy layout) and `adapters::hermes::HermesProfile` (id `hermes`; `write` is the plain mapping:
SOUL.md, memories/, skills/, cron jobs in `cron/jobs.json` always paused, MCP connectors with an http url in
`config.yaml` `mcp_servers`, and credential names, other connectors and `manual` items in `botport-manual.md`;
`read` maps them back and never reads `.env` or `auth.json`). `HermesProfile` is also the one `Target`: live
profiles get cron jobs through the hermes CLI, created paused unless the spec job says `enabled: true`.
`battery init`, `export`, `verify` and `describe` still work on the Grok bundle layout directly.

The core must not name a platform: `adapter_tests::core_has_no_platform_names` fails if any file in `src/core/`
contains `grok` or `hermes`.

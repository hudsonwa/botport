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

## The `Adapter` trait (`src/core/adapter.rs`)

```rust
pub trait Adapter {
    fn id(&self) -> &'static str;                                  // also its key in BotModel::extras
    fn read(&self, src: &Path) -> Res<BotModel>;                    // native layout -> model
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>>; // model -> native layout
}
```

Implementations: `adapters::grok_bot::GrokBot` (id `grok-bot`; reads an export bundle or legacy hand export,
writes the legacy layout) and `adapters::hermes::HermesProfile` (id `hermes`; reads/writes SOUL.md, memories/,
skills/). Today the CLI commands (`plan`, `apply`, `sync`, `export-hermes`, `drift`, ...) call the Hermes adapter
modules (`src/adapters/hermes/`) directly rather than going through the trait; routing them through `Adapter` is
open work.

The core must not name a platform: `adapter_tests::core_has_no_platform_names` fails if any file in `src/core/`
contains `grok` or `hermes`.

# Documentation

Start with the [main README](../README.md) for what botport is and a quick try-out.

## Using botport
- [INSTALL.md](INSTALL.md): install the binaries.
- [USER-GUIDE.md](USER-GUIDE.md): export, import, sync, test and undo, step by step.
- [examples/daily-wisdom.md](examples/daily-wisdom.md): a worked example, from a real template to a passing battery.
- [REFERENCE.md](REFERENCE.md): every command, the bundle layout, the port spec and its ownership rules.

## Briefs for agents
Hand these to an agent and it can do the job for you.
- [AGENT-SETUP.md](AGENT-SETUP.md): set up the g2h talk bridge.
- [AGENT-EXPORT.md](AGENT-EXPORT.md): a Grok Bot exports itself (or coordinates other bots).
- [AGENT-IMPORT.md](AGENT-IMPORT.md): a Grok Bot loads what the Hermes bot learned.
- [ROUTINE-PROMPT.md](ROUTINE-PROMPT.md): paste-ready prompts for the Grok side of g2h.

## How it works
- [ARCHITECTURE.md](ARCHITECTURE.md): core and adapters, the `Adapter` trait.
- [MODEL.md](MODEL.md): the neutral bot model and how each platform maps onto it.
- [ADAPTERS.md](ADAPTERS.md): write an adapter for another platform; conformance tests.
- [SECURITY.md](SECURITY.md): the g2h threat model and its known limits.
- [PLATFORM-CHANGES.md](PLATFORM-CHANGES.md): Grok Bot and Hermes behaviour changes, and how botport adapted.
- [CI.md](CI.md): continuous integration.
- [g2h README](../crates/g2h/README.md): the talk bridge crate.

## Glossary
- **botport**: the porting tool (`plan`, `apply`, `sync`, `drift`…). **g2h**: the talk bridge (`g2h` CLI, `g2hd` daemon).
- **g2h / h2g** (as sync directions): Grok → Hermes and Hermes → Grok. `drift` reports each one.
- **bundle**: what `export` writes from a Grok Bot (skills, memory, history, checklist); the input to `plan`/`apply`.
- **owned leaves**: the parts of the Hermes profile the port spec says botport manages; everything else is left alone.
- **conflict**: an owned part edited on the Hermes side since the last apply. It is reported, never overwritten.
- **manual item**: something that can't be copied as a file (a connector, a credential); it goes on the checklist.
- **conv**: a g2h conversation (`conv_id`), the thread a set of messages belongs to.
- **hop**: how many bot-to-bot replies deep a message is; the hop limit (default 3) stops loops.

## Project
- [CONTRIBUTING.md](../CONTRIBUTING.md), [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md),
  [ROADMAP.md](../ROADMAP.md), [CHANGELOG.md](../CHANGELOG.md).

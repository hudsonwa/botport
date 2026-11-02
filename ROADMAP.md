# Roadmap

botport is at v0.1. This page lists what comes next and, more importantly, the **areas that need an owner**. An
owner reviews PRs in that area, keeps its docs true, and decides small design questions. If one of these interests
you, say so on an issue or in a PR.

## Areas that need an owner
### The Grok side
Grok Bots have no export API, so the bot exports itself by following [docs/AGENT-EXPORT.md](docs/AGENT-EXPORT.md),
and loads updates with [docs/AGENT-IMPORT.md](docs/AGENT-IMPORT.md). This needs someone who uses Grok Bots daily to:
- keep the briefs working as the Grok platform changes;
- grow the grok-bot-export v1 support (`src/adapters/grok_bot/export_json.rs`);
- port more public templates (see the [worked example](docs/examples/daily-wisdom.md)).

### Hermes adapters
The Hermes adapter (`src/adapters/hermes/`) owns parts of a profile and must never overwrite a user's edit. Open work:
- keep data the model can't place yet instead of dropping it;
- follow new Hermes releases (config keys, cron, skills layout);
- the `shim/` source for NousResearch agent-importers.

### g2h, the talk bridge
`crates/g2h` is a local broker between the two bots. Open work:
- run and test the systemd user unit on more Linux distributions;
- a status view (`g2h status` as a small TUI);
- a clear answer for Windows (it uses Unix sockets today).

## Next platforms
- **OpenClaw** and **OpenBot** adapters. Their mappings are already written up in [docs/MODEL.md](docs/MODEL.md).

## Planned: a development tier for ops
Not implemented yet. The idea: let the Grok-side bot run a few development ops (for example applying an update or
porting a template) at the auto tier, reading files only from one owner-chosen updates directory, with:
- a kill switch (a `PAUSE` file in the g2h home stops all such ops);
- per-day and per-op time caps;
- no money, messages, credentials or deletion;
- revocation by the owner at any time.

Anything that can write to the updates directory could direct the bot, so this needs a careful design first.

## Not planned
- Copying secrets or credentials between platforms. botport lists their names; you re-enter the values.
- A hosted service. botport stays a local tool.

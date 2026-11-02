# Roadmap

botport is an early v0.1 built by one maintainer. This page says what works, what is being built, and where help
would matter most.

## Status
| Item | Status |
|---|---|
| Grok Bot ↔ Hermes export/import, sync and drift | done |
| g2h talk bridge, running as your user | done |
| Neutral `BotModel` + `Adapter` seam routing plan/apply/sync/drift | done (v0.1.0) |
| OpenClaw (formerly Moltbot/Clawdbot) adapter | planned |
| OpenBot adapter | planned |
| `g2hd` running as a separate OS user | planned |

## Help wanted
1. **A co-maintainer who runs Grok Bots daily**, to own the export brief and the Grok side. Grok Bots have no export
   API, so the bot exports itself by following [docs/AGENT-EXPORT.md](docs/AGENT-EXPORT.md) and loads updates with
   [docs/AGENT-IMPORT.md](docs/AGENT-IMPORT.md). Those briefs depend on paths and behaviour inside a closed platform;
   fixture tests stay green when it changes. This needs someone who sees the changes first, keeps the briefs working
   and records each change in [docs/PLATFORM-CHANGES.md](docs/PLATFORM-CHANGES.md).
2. **Rust contributors for adapters.** plan/apply/sync/drift now go through `BotModel` + `Adapter`, so a new
   adapter that reads a platform into the model can be ported to Hermes. OpenClaw (#1) and OpenBot (#2) are the next
   adapters.

Smaller open work: keep data the model can't place yet instead of dropping it; follow new Hermes releases; test the
g2h systemd user unit on more Linux distributions; an answer for Windows (g2h uses Unix sockets today).

## Next platforms
- **OpenClaw (formerly Moltbot/Clawdbot)** adapter: planned.
- **OpenBot** adapter: planned.

These two are what the `BotModel` seam is for. plan/apply/sync/drift go through it now, so an adapter that reads a
platform into the model can port that bot to Hermes. Their mappings are
written up in [docs/MODEL.md](docs/MODEL.md); no code yet.
- **`g2hd` as a separate OS user** (planned). Today g2h runs as your user and is not a security boundary against a
  bot with a shell as that user; see [docs/SECURITY.md](docs/SECURITY.md).

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

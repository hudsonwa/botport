# Roadmap

botport is an early v0.1 built by one maintainer. This page says what works, what is being built, and where help
would matter most.

## Status
| Item | Status |
|---|---|
| Grok Bot ↔ Hermes export/import, sync and drift | done |
| g2h talk bridge, running as your user | done; optional, experimental companion, feature-frozen for v0.1 |
| Neutral `BotModel` + `Adapter` seam routing plan/apply/sync/drift | done (v0.1.0) |
| OpenClaw (formerly Moltbot/Clawdbot) adapter | planned |
| OpenBot adapter | planned |
| `g2hd` running as a separate OS user | implemented, opt-in (not yet run in production) |

## Help wanted
1. **A co-maintainer who runs Grok Bots daily**, to own the export brief and the Grok side. Grok Bots have no export
   API, so the bot exports itself by following [docs/AGENT-EXPORT.md](docs/AGENT-EXPORT.md) and loads updates with
   [docs/AGENT-IMPORT.md](docs/AGENT-IMPORT.md). Those briefs depend on paths and behaviour inside a closed platform;
   fixture tests stay green when it changes. This needs someone who sees the changes first, keeps the briefs working
   and records each change in [docs/PLATFORM-CHANGES.md](docs/PLATFORM-CHANGES.md).
2. **Small, concrete fixes first.** The recommended first contributions are a
   [PLATFORM-CHANGES](docs/PLATFORM-CHANGES.md) entry from a bot you run, #16 (Hermes adapter write: connector key
   order, repeated -N duplicates, stale credential ticks) and #5 (more parity-battery cases). Read
   [docs/OWNERSHIP.md](docs/OWNERSHIP.md) first: it is the spec of the ownership rules every change must keep.
3. **Rust contributors for adapters, later.** OpenClaw (#1) and OpenBot (#2) are large pieces of work, not entry
   points; see "Next platforms" below for what they need.

Smaller open work: keep data the model can't place yet instead of dropping it; follow new Hermes releases; test the
g2h systemd user unit on more Linux distributions; an answer for Windows (g2h uses Unix sockets today).

## Next platforms
- **OpenClaw (formerly Moltbot/Clawdbot)** adapter: planned.
- **OpenBot** adapter: planned.

These two are what the `BotModel` seam is for; neither has code yet. What a contributor must build today: an adapter
that reads the platform into the model (and writes it back) and passes the conformance suite, plus a CLI source
selector (the CLI only takes `--bundle`, the Grok Bot source). Porting *into* either platform also needs a `Target`
for it (Hermes is the only one) that keeps the rules in [docs/OWNERSHIP.md](docs/OWNERSHIP.md). Their mappings are
written up in [docs/MODEL.md](docs/MODEL.md).

## g2h scope
g2h (`g2h` CLI, `g2hd` daemon) is an optional, experimental companion to botport, and is **feature-frozen for v0.1**:
bug fixes only, no new features. Separate-user hardening (#17) is post-launch.
- **`g2hd` as a separate OS user** (implemented, opt-in, not yet run in production). By default g2h runs as your
  user and is not a security boundary against a bot with a shell as that user; the opt-in `[separate_user]` mode
  and its setup recipe are in [docs/G2H-SEPARATE-USER.md](docs/G2H-SEPARATE-USER.md).

## Planned after v0.1: a development tier for ops
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

# Prior art

botport's Grok engine (`botport grok …`, crate `crates/botport-grok`) builds on ideas from six earlier projects that
drive or bridge the Grok Bot desktop app. We thank their authors.

**Clean-room statement.** The engine is a clean-room implementation. It was written from these projects' public
READMEs and docs and from our own probing of the app (Grok Bot 1.1.0 on a test account). botport's authors did not
read, copy or adapt their source code. Where a README and our probe disagreed, the probe won.

**Compatibility on purpose.** The command names (`bots`, `ask`, `send`, `wait`, `read`, `watch`, `stop`, `launch`,
`restore`, `usage` and `routines`; `create` and `delete` for bots are not in botport yet) and the exit codes 0, 2 and 3 match
TheSethRose/grokbot-bridge on purpose, so scripts written for it keep working. Exit codes 1, 4 and 5, the presets, the
hard floors and the tested-versions canary are botport's own.

| Project | What its README taught us | What our probe found / what we did |
|---|---|---|
| [TheSethRose/grokbot-bridge](https://github.com/TheSethRose/grokbot-bridge) | Command names; exit 0/2/3; `--settle` 8 s; routine add/enable/disable/remove/run; downloads never overwrite; delete needs the exact name; the debug-port risk and `launch`/`restore` | Confirmed. `stop` differs: 1.1.0 has no Stop control, so botport stops with a steer message. The port risk is wider than described, which led to hard floors 5 (port TTL) and 6 (IPC allowlist) |
| [rthomas24/grok-bot-mcp](https://github.com/rthomas24/grok-bot-mcp) | An accessibility (AX) route that needs no debug port; choice questions | Planned as the `ax` transport, the fallback after the debug port |
| [dimpurr/grok-bot-gateway](https://github.com/dimpurr/grok-bot-gateway) | The webhook route: one key reaches every bot; return paths; cost per call | Not probed; the `webhook` transport is parked (P6) and its key is stored by reference |
| [kinopeee/grokbot-mcp-bridge](https://github.com/kinopeee/grokbot-mcp-bridge) | A synchronous ask over webhooks with run ids; webhook keys kept server-side | The key shape is in our privacy gate's patterns |
| [quabug/grok-bot-mcp](https://github.com/quabug/grok-bot-mcp) | An HTTP MCP server behind OAuth; a structured output schema per tool | botport stays stdio-only for MCP; tool results carry the same structured JSON as the CLI |
| [DaizeDong/grokbot-cdp](https://github.com/DaizeDong/grokbot-cdp) | "Measured, never inferred": a channel that can lie must fail instead | Adopted as the rule for botport's PASS lines: each PASS is checked by a second route |

The rest of the engine (state mapping, the IPC allowlist, steer-stop, the version gate and canary, presets and
floors) comes from our own probe and plan, not from these projects. Their licences are their own; botport includes
none of their code.

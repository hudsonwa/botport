# Neutral bot model — spec v1 (`bot-model/1`)

Every adapter reads a platform's export **into** this model and writes a platform's import **from** it
(hub and spoke). The core knows no platform. Rust types: `src/core/model.rs`. Fixtures: `tests/fixtures/model/`.
JSON Schema: [`schema/botmodel-v1.json`](schema/botmodel-v1.json) (draft 2020-12). `tests/schema.rs` validates
every model fixture and every model the adapters read against it, and checks both directions for the model and every
`$defs` object: it fails when the types have a field the schema lacks, when the schema lists a property the types
lack, and when the schema's `required` differs from the fields the types require. The crate API (`cargo doc --no-deps --open`) documents the same types.

## Document

One JSON object. Canonical form: UTF-8, 2-space pretty-printed, trailing newline, fields in the order below,
map keys sorted. Empty lists/maps and absent optionals are omitted. A canonical document round-trips byte-identically.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `schema` | string | yes | `bot-model/MAJOR[.MINOR]`, currently `bot-model/1` |
| `persona.name` | string | yes | display name of the bot |
| `persona.instructions` | string | yes | role / system prompt / persona text (may be empty) |
| `memory[]` | Fact | no | remembered facts, in origin order |
| `skills[]` | Skill | no | reusable instruction packs |
| `schedules[]` | Schedule | no | timed or event-triggered jobs |
| `connectors[]` | Connector | no | tool servers (MCP etc.) |
| `credentials[]` | CredentialRef | no | names of secrets the bot needs — **never values** |
| `memory_text` | string | no | the origin's memory as one document, verbatim |
| `history` | HistoryRef | no | pointer to conversation history stored beside the model |
| `manual[]` | ManualItem | no | what the reader could not carry, for a human (names and reasons only) |
| `extras` | map adapter-id → any JSON | no | data an adapter could not map; never drop silently |

**Fact** — `text` (required); optional `tier` (`core` = always in context, `recall` = searchable), `scope`
(`user`, `agent`, `project`, …), `source` (origin location, e.g. a file or table name), `date` (ISO-8601),
`origin_side` (opaque id of the side that first wrote it, for two-way sync).

**Skill** — `name`; optional `description`; `files` map of relative path → UTF-8 text; `binary_files` map of
relative path → lowercase hex, for files that are not UTF-8.

**Schedule** — `name`, `cron` (5-field; omitted for events), `tz` (IANA), `kind` (`scheduled` | `event`),
`action` (prompt to run), `enabled` (bool, always written).

**Connector** — `name`, `kind` (`mcp-http`, `mcp-stdio`, …), optional `url`, `tools[]` (allowed tool names).

**CredentialRef** — exactly `name` (names only). A `where` field from older documents is accepted and dropped, never
written. Other unknown fields are rejected, so a document carrying a `value`/`secret` field fails to parse.

**HistoryRef** — `kind` (e.g. `jsonl`, `sqlite`), `path` (relative to the model file).

**ManualItem** — `item` (e.g. `schedule Nightly`), `todo` (what to do by hand), optional `done` (bool, ticked off
by a human; omitted when false). Written to the target's checklist (e.g. `botport-manual.md`).

`memory_text` and `manual[]` were added after the first documents were written; both are optional and omitted when
empty, so older documents round-trip byte-identically and the schema stays `bot-model/1`. `memory_text` is e.g. a
Grok bundle's MEMORY.md; `manual[]` holds schedules that run elsewhere, apps without an MCP url and entries that
looked like secrets.

## Versioning

The schema file is versioned by major: `docs/schema/botmodel-v1.json` describes `bot-model/1` and only ever
changes additively (a new optional field, together with the types). A breaking change gets a new major,
`bot-model/2`, and a new file `botmodel-v2.json`; the v1 file stays.

- Minor (`bot-model/1.N`): additive only — new optional fields. Readers of major 1 accept any 1.x. Unknown
  **top-level** data from a newer minor is kept in `extras["_unknown"]` (one entry per field) by `BotModel::from_json`;
  unknown fields **inside** nested objects (a persona, skill, schedule …) are not kept today, so a newer minor should
  add new data at the top level or in `extras` until that changes.
- Major (`bot-model/2`): anything breaking (rename, removal, type or meaning change). Readers reject any major
  they do not know with `unsupported major version N`.
- Other schema families are rejected.

## Mapping tables

### 1. botport bundle (`src/adapters/grok_bot/bundle.rs`, `docs/AGENT-EXPORT.md`)

| Bundle | Model | Lossy notes |
|---|---|---|
| `profile.json` `name` | `persona.name` | — |
| agent instructions / role text | `persona.instructions` | — |
| `memory-agent.jsonl` rows | `memory[]` (`source` = file, `date` from row) | tier not recorded → omitted |
| `MEMORY.md` facts | `memory[]` `tier: core` | free-form prose split per entry |
| `MEMORY.md` as a whole | `memory_text` | verbatim |
| `skills/<name>/**` | `skills[]` `files`, `binary_files` (hex) | — |
| scheduled tasks | `schedules[]` | — |
| `describe.md` sections (what can't be copied) | `extras["grok-bot"]` | human prose, not structured |
| history export | `history` | stays a separate file |
| excluded paths list | `extras["grok-bot"].excluded` | — |

### 1b. The bot's description (`NOT_CAPTURED.md` → `normalized/describe.json`; code: `src/adapters/grok_bot/describe.rs`, `from_describe` in `mod.rs`)

Read from `normalized/describe.json` when present, else parsed from the bundle's `NOT_CAPTURED.md` (for a
grok-bot-export file: a `describe.json` beside it). Merged with the export's routines/apps, deduped by name (export wins).

| describe.json | Model | Lossy notes |
|---|---|---|
| `schedules[]` `name`/`cron`/`timezone`/`prompt` (`side: grok`) | `schedules[]` `kind: scheduled`, `tz` (empty when not given), `action`, `enabled` (false when `paused`, set when the line says "(paused)") | suspicious schedules are in `manual[]`, not here |
| `schedules[]` `side: other` | `manual[]` `schedule <name>` | runs off Grok: moved by hand |
| `connectors[]` `kind: mcp` + http(s) `url` | `connectors[]` `kind: mcp-http`, `tools` | suspicious connectors are in `manual[]`, not here |
| `connectors[]` other kinds (`builtin`, `url`, `unknown`, `mcp` without URL) | `connectors[]` with that kind | not set up automatically |
| `credentials[]` `name` | `credentials[]` `name` | names only; re-checked on load (a name that may hold a value goes to `manual`) |
| `unparsed[]` | `extras["grok-bot"].describe_unparsed` (count) | needs a human |
| `manual[]` `kind`/`name`/`line`/`reason` | `manual[]` `item` (kind and name), `todo` (reason) | looked like it holds a secret: text not copied |

describe.json keeps structured fields only (no `raw` line text). Text is never rewritten (`src/core/secrets.rs`,
see USER-GUIDE and SECURITY): schedule and connector lines and credential names go through rule (A), keyword routing,
so one that merely mentions a secret keyword (e.g. "token budget") is routed to the manual checklist with the keyword
as its reason; every other line, and every written string, goes through rule (B), the high-precision write guard
(token shapes, url credentials, a secret-named `key=value`; no entropy check), with no keyword rule.

Write (Grok side): the model's schedules/connectors/credentials become a `NOT_CAPTURED.md` description in the same
form, which reads back to the same model (event routines as prose).

### 2. grok-bot-export v1 (upstream agent-importers `grok_bot.py`; code: `src/adapters/grok_bot/export_json.rs`)

Input: one JSON document `{"format": "grok-bot-export", "version": 1, "bots": [...]}`. The first bot is read
(`extras["grok-bot"].other_bots` counts the rest). Any secret-looking key anywhere refuses the whole document.

| Export key (per bot) | Model | Lossy notes |
|---|---|---|
| `name` | `persona.name` | — |
| `instructions` | `persona.instructions` | text with literal `\n` and no real newline is unescaped (as upstream `_text`) |
| `title` | `extras["grok-bot"].title` | — |
| `memories[]` | `memory[]` `tier: core`, `scope: agent` | no dates; `[truncated …]` markers kept, counted in `extras["grok-bot"].truncated_memories` |
| `user_facts[]` | `memory[]` `tier: core`, `scope: user` | — |
| `skills[]` `name`/`description`/`instructions` | `skills[]` named by upstream slug, one `SKILL.md` byte-identical to upstream's | skills without instructions skipped |
| `routines[]` trigger `schedule`/`cron`/… with 5-field `schedule_cron` | `schedules[]` `kind: scheduled`, `tz` = `timezone` (default `UTC`) | — |
| `routines[]` other triggers (Slack, GitHub, email, webhook) | `schedules[]` `kind: event`, no `cron` | target may not run it |
| `routines[].paused` | `enabled = !paused` | — |
| `routines[]` `trigger`, `schedule_text`, `trigger_details`, `paused_reason`, `skills` | `extras["grok-bot"].routines[<name>]` | — |
| `apps[]` with http(s) `mcp_url` | `connectors[]` `kind: mcp-http`; name = short id (`id` if given, else the app name before `(`/`;`/` - `, slugged, ≤32 chars) | — |
| `apps[]` without URL | `extras["grok-bot"].apps_without_url` and `manual[]` `connector <name>` | logins stay on origin |
| secret-looking keys (`token`, `password`, `api_key`, …) | refused | never carried |

### 3. OpenBot (upstream agent-importers `openbot.py`, SQLite projections)

| Source | Model | Lossy notes |
|---|---|---|
| `projection_agents.agent_json` name/instructions | `persona` | other agent_json keys → `extras` |
| `projection_agent_memories.text` (by `created_at`) | `memory[]` with `date` | — |
| `projection_agent_routines` `name`/`instruction`/`active`/`timezone` | `schedules[]` | — |
| `projection_routine_triggers.schedule_json` (`kind`, `time`, `weekday`, `amount`/`unit`, `expression`) | `schedules[].cron` | interval/anchor schedules not expressible in cron → `extras` |
| `projection_mcp_servers` `transport`/`url`/`command`/`args_json` | `connectors[]` | `command`, `cwd`, `env_json` → `extras`; env **values** dropped, names → `credentials[]` |
| `headers_json` | names → `credentials[]` | header values never carried |

### 4. OpenClaw (upstream agent-importers `openclaw.py`, `openclaw_to_hermes.py`)

| Source | Model | Lossy notes |
|---|---|---|
| `SOUL.md` / workspace agent files | `persona.instructions` | per-agent multi-workspace → one model per agent |
| `MEMORY.md`, `USER.md`, daily memory notes | `memory[]` (`scope` user/agent, `date` from daily file) | — |
| workspace + shared skills | `skills[]` | — |
| cron jobs config | `schedules[]` | — |
| `mcp` servers config | `connectors[]` | — |
| provider keys, channel tokens (`apiKey`, …) | names → `credentials[]` | values never carried |
| model, TTS, channels (Discord/Slack/WhatsApp/Signal), browser, approvals, hooks, plugins, command allowlist | `extras["openclaw"]` | platform settings, not portable |

## Lossy / not portable

- Secret values of any kind (only names travel).
- Model/provider choice, reasoning settings, TTS voice, gateway/channel bindings, approval policies, hooks, plugins.
- Event triggers whose source platform the target cannot subscribe to (kept as `kind: event`, possibly disabled).
- Non-cron schedules (intervals anchored to a time, "every N months on weekday") → `extras`.
- Logins/OAuth sessions of hosted apps; conversation history content
  (referenced, not embedded).

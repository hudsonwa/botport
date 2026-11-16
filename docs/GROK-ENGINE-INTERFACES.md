# Grok engine interfaces (frozen at the end of P1)

Status: **frozen** for P2 (twin), P3 (routines, usage, files), P4 (create/rename/delete) and P5 (canary,
versions). Changes after this point are additive only (new `Cap` values, new default trait methods, new optional
JSON fields). Anything else needs a note in this file and a heads-up to every lane that builds on it.

Code: `crates/botport-grok` (a workspace crate), linked into the `botport` binary through the default cargo
feature `grok` (`cargo build --no-default-features` leaves it out). The export/apply core does not depend on it.

## 1. CLI contract (what twin calls)

```
botport grok status | launch [--port-ttl 60m] | restore
botport grok bots
botport grok send <bot> <message...>
botport grok ask  <bot> <message...> [--timeout 300] [--settle 8]
botport grok wait <bot> [--since KEY] [--timeout 1800] [--settle 8]
botport grok read <bot> [--limit N] [--since KEY]
botport grok stop <bot> [--steer TEXT]
botport grok watch [bot] [--for DUR] [--extend]
botport grok policy show | preset careful|balanced|fast | set KEY VALUE
botport grok mcp [--bot-callable]
botport grok canary                                  (P5, additive; `--write` exits 1 until P4)
common: --json --quiet --transport auto|debug-port|fake --trust-version --no-log
fake:   --transport fake --fixtures DIR [--turn NAME]... [--scenario idle|awaiting]   (or BOTPORT_GROK_FAKE=DIR)
```

- `<bot>`: exact id, exact name (case-insensitive) or a unique name prefix. Ambiguous or unknown: exit 1.
- `botport grok list` is not part of the engine; it stays with the twin interface (`docs/TWIN.md`).
- `BOTPORT_HOME` (default `~/.botport`) holds `grok-policy.toml`, `grok-port.json` and `grok-actions.jsonl`, all 0600.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | done; for ask/wait the bot is idle with a reply after our message |
| 1 | other failure: bad arguments, ambiguous bot, parse error, shape not recognised, a PASS check that disagreed |
| 2 | timeout and the bot is still working (or no reply yet): run `wait` again |
| 3 | the bot is waiting for the user (approval card or question); see `bot.needs_input` |
| 4 | refused by policy or a hard floor; the line names the setting (incl. a write on an untested version: `unknown_version_writes`) |
| 5 | app or transport unavailable (not installed, port closed and launch not allowed, the canary's port check failed) |

**Change (P5 lane, owner-directed):** a write on a version outside the tested table now exits **4** (refused by the
`unknown_version_writes` setting), not 5 as P1 shipped. Callers that retried on 5 for this case should treat it as
"run `botport grok canary`". This is the only non-additive change since the freeze.

### Output

- Human: one line per item, then the proof line `PASS: <what>; <check>` or `FAIL: <what>; <why>` as the last line
  on stdout (a `hint:` line before a FAIL).
- `--json`: one JSON object on stdout; the proof line is repeated on stderr. `watch --json` prints JSON lines and a
  last `{"type":"summary",…}` line.
- Every JSON object carries `exit` and `proof`; failures without a value carry
  `error: {code, message, hint}`; `warnings: [..]` when there are any.

| Command | JSON (besides exit, proof) |
|---|---|
| status | `installed, version, tested, running, pid, port_open, port_by_botport, port_opened_at, port_expires_at, preset`, optional `local_tool_permission` |
| launch | `pid, opened_at, expires_at` (never the port number or websocket path) |
| bots | `bots: [Bot]` |
| send | `bot: Bot, key` |
| ask / wait | `bot: Bot, messages: [Message], state, waited_s` (messages = non-user entries after our message) |
| read | `messages: [Message]` |
| stop | `bot: Bot, method: "steer"\|"native", state, idle_after_s` |
| watch line | `type: working\|idle\|needs_input\|message, bot, bot_id, at, message: Message\|null` |
| policy | `policy: {preset, bots_scope, confirm_destructive, debug_port, port_ttl_s, reply_wrapping, …}` |
| canary (P5) | `version, tested, transport, at, pass, read_passed, read_total, steps: [{name, covers, pass, ran, detail}], report` |

`ask`/`wait`/`read` JSON is wrapped per `reply_wrapping`: `raw` (fast) as above; `data` →
`{"untrusted_bot_content": <object>}`; `note` → the same plus `"note"`.

```jsonc
// Bot
{"id":"<uuid>","name":"…","main":false,"state":"idle|working|needs_input","activity":"is …"|null,
 "needs_input":null|{"kind":"approval|question","reason":"…","since":"<rfc3339>"|null,"options":["…"]},
 "unread":0,"last_key":"t45s2","last_at":"<rfc3339>"|null,"hidden":false}
// Message
{"key":"t45s2","role":"user|bot|event","kind":"text|question|approval|routine_event|feedback",
 "text":"…","at":"<rfc3339>"|null,"links":["…"],"file":null,"options":[{"label":"…","value":"…"}]|null,
 "answered":null|"<value>|skipped|allowed|denied…"}
```

Times are RFC 3339 UTC in every JSON field. Human output (watch lines, `status`, `launch`, and the times inside
PASS/FAIL lines) shows local time with its zone, e.g. `2026-11-16 11:59:35 AWST` or `until 12:59 AWST` (P5 lane,
additive: P1 printed UTC there). Since the proof line is also the JSON `proof` string, a time inside it is local and
always zone-labelled; parse the RFC 3339 fields, not the proof text.

### PASS checks (second route)

| Command | Did the work | PASS checked by (`proof.route`) |
|---|---|---|
| bots | sidebar props (`agents[]`) | sidebar DOM rows: same ids and names (`dom-rows`) |
| send | composer input | the transcript props hold our user message (`transcript`) |
| ask / wait | agent flags (settle loop) | the bot's sidebar row is neither composing nor waiting (`dom-rows`) |
| stop | steer message + flags | sidebar row idle (`dom-rows`) |
| read / watch | props | `self-check` |
| status | process table | `DevToolsActivePort` vs the listener table (`process-table`) |
| launch / restore | app restart | `lsof` listener table (`lsof`) |
| policy | policy file write | the saved file read back (`policy-file`) |
| canary | read-only probes | each probe's own PASS route; the report file (`canary`) |

## 2. Transport

```rust
pub enum Cap { ListBots, Send, Attach, StateFlags, NeedsInput, ReadAny, ReadOpen, Stop, RoutinesList,
               RoutinesToggle, RoutinesAddViaBot, RoutinesRemove, Usage, Files, Lifecycle, Rows,
               PortCheck /* P5, additive */ }
pub struct Capabilities { pub caps: BTreeSet<Cap>, pub app_version: Option<String>, pub tested: bool }

pub enum TransportError {
    Unavailable(String),        // exit 5
    BotComputerAsleep(String),  // exit 5
    Shape { field, version },   // exit 1, names the field
    Unsupported(Cap),           // engine tries the next transport; exit 5 if none is left
    App(String),                // exit 1 (scrubbed text)
    Floor(String),              // exit 4: a floor stopped it at the transport edge
}

pub trait Transport: Send {
    fn name(&self) -> &'static str;                 // "debug-port" | "ax" | "bot-shell" | "webhook" | "fake"
    fn probe(&mut self) -> TResult<Capabilities>;
    fn port_check(&mut self) -> TResult<String>;                         // P5, additive: route still opens
    fn bots(&mut self) -> TResult<Vec<Bot>>;                               // props route
    fn rows(&mut self) -> TResult<Vec<RowView>>;                           // second route (DOM / AX)
    fn bot_state(&mut self, id: &str) -> TResult<BotSnapshot>;             // cheap poll
    fn open(&mut self, id: &str) -> TResult<()>;                           // default Ok(())
    fn send(&mut self, id: &str, text: &str, files: &[PathBuf]) -> TResult<MessageKey>;
    fn read(&mut self, id: &str, since: Option<&str>, limit: usize) -> TResult<Vec<Message>>;
    fn stop(&mut self, id: &str, steer: &str) -> TResult<StopMethod>;      // default: send(steer) -> Steer
    fn routines(&mut self, id: &str) -> TResult<Vec<Routine>>;                          // P3
    fn routine_set_enabled(&mut self, id: &str, r: &str, on: bool) -> TResult<Routine>; // P3
    fn routine_remove(&mut self, id: &str, r: &str) -> TResult<()>;                     // P3
    fn usage(&mut self) -> TResult<Usage>;                                              // P3
    fn files / file_attach / file_remove / file_read                                    // P3
    fn create / rename / delete(&mut self, id: &str, exact_name: &str)                  // P4 (floor 4 rechecked)
}
```

Every method except `name` and `probe` has a default that returns `Unsupported(cap)`.

- `BotSnapshot { state, activity, needs_input, last_key, newest_bot_entry: Option<EntryKind> }`,
  `EntryKind = Text | PendingApproval | OpenQuestion | Other`.
- `RowView { id, name, main, awaiting, composing, desc }` (the DOM/AX row: aria-label, aria-description,
  data-awaiting, data-composing, data-layout=pinned for the main bot).
- **State mapping** lives in `parse::snapshot` and is shared by every transport: `awaitingUserResponse` →
  needs_input; else `isRunningTurn || isRunning || isComposingMessage` (or the chat's running/composing) → working;
  else a newest bot entry that is a pending approval or an unanswered question → needs_input (source `Entry`); else
  idle. An approval whose `since` is older than our message is stale (a new message cancels it) and is ignored.
- **Parsers** (`parse::roster`, `parse::agent`, `parse::chat`, `parse::entry`, `parse::row`) take the app's raw
  props JSON; required fields per version are in `version::fields_for`. Transports must go through them.

### debug-port transport (`transport::cdp`)

- Connects only through `~/.botport/grok-port.json`, only when the listener is the Grok main pid on 127.0.0.1, only
  to the single `page` target.
- CDP methods allowlisted: `Runtime.evaluate`, `Input.insertText`, `Input.dispatchKeyEvent`,
  `Input.dispatchMouseEvent`.
- IPC only through `CdpTransport::ipc(method, args)`, which enforces the floor-6 allowlist
  (`policy::floors::IPC_READ` / `IPC_WRITE`). P3 adds its calls there, never around it.
- P1 caps: ListBots, Rows, Send, StateFlags, NeedsInput, ReadOpen, Stop. `read` needs the bot open on screen
  (`open` clicks its sidebar row). Attachments: `Unsupported(Attach)` until P3.

### Fake transport (`transport::fake::FakeTransport`) — what lane C builds against

```rust
FakeTransport::load(dir, clock)            // roster.json; transcript-idle.json is the first (main) bot's chat
FakeTransport::load_awaiting(dir, clock)   // transcript-awaiting-approval.json (bot needing approval)
  .with_turn("reply-ready")                // queued; the next send() starts it (repeatable)
  .with_running(bot_id, "tool-run-escape-ignored")?  // a turn already running, from its first working sample
  .with_version("9.9.9")                   // version-gate tests
  .fail_next(Cap::ListBots, TransportError::Unavailable(..))  // fault injection
  .writes()                                // RecordedWrite::Send{bot,text} | Stop{bot,steer}
```

Turn scripts (`tests/fixtures/app-1.1.0/turns/`): `reply-ready` (idle → reply "READY", exit 0), `long-reply`
(still composing at the end, exit 2), `tool-run-escape-ignored` (still running, exit 2), `needs-approval` (exit 3),
`routine-created` (routine event + "ROUTINE OK", exit 0), `steer-stop` (running → idle with "STOPPED").
Samples are 400 ms apart in virtual time ([`clock::FakeClock`]); recorded times are rebased to the send time.
From the CLI, the fake runs in one process: `send` then `wait` in two processes do not share fake state, so use
`ask`, or drive the engine as a library.

## 3. Policy

```rust
pub enum Preset { Careful, Balanced, Fast }          // default Fast
pub struct Policy { preset, bots_scope: BotsScope /* All | Allowlist(Vec<String>) */,
    confirm_destructive: Confirm /* Typed | YesFlag | None */, debug_port: PortMode /* Off | AskOnce | Auto */,
    port_ttl_s: u64, reply_wrapping: Wrap /* Note | Data | Raw */, action_log: bool,
    secrets_to_grok: SecretMode /* ByRef | ValuesWithFlag */, proof: ProofLevel /* Full | Fast */,
    usage_check: UsageCheck /* Refuse(pct) | Warn(pct) */, unknown_version_writes: VersionGate,
    mcp_lifecycle_tools: bool, transports: TransportSet, launch_remembered: bool }
pub enum Caller { Cli { tty: bool }, Mcp { bot_callable: bool } }
pub enum Action<'a> { Read, Send, Stop, Watch, RoutineChange, Delete { exact, confirm, yes }, Launch, AutoLaunch,
                      SecretValue { flag }, Ipc(&str), AnswerApproval, Bind(&str) }
pub enum Decision { Allow, Ask(String), Refuse { setting: &'static str, hint: String } }   // Refuse -> exit 4
impl Policy { fn preset(Preset) -> Self; fn decide(&self, &Caller, &Action, Option<&Target>) -> Decision;
              fn load(home) / save(home) / set(key, value) }
```

| Setting | careful | balanced | fast (default) |
|---|---|---|---|
| bots_scope | allowlist (empty) | all | all |
| confirm_destructive | typed (CLI only; MCP refuses) | `--yes` / MCP confirm = exact name | none (floor 4 applies) |
| debug_port | off | ask once on a TTY, then remembered | auto launch |
| port_ttl | 15 min | 30 min | 60 min |
| reply_wrapping | note | data | raw |
| action_log | on | on | on (`--no-log` / `action_log off`) |
| secrets_to_grok | by reference | by reference | values with `--allow-secret-values` |
| usage_check | refuse ≥ 90 % | warn ≥ 80 % | warn ≥ 90 % (P3 wires the check) |
| usage_budget_week | none | none | none (paid turns botport may spend in 7 days) |
| usage_warn_pct | 80 | 80 | 80 (warn at this % of the budget) |
| usage_hard_cap | on | on | on (at the budget: exit 4; off: warn) |
| unknown_version_writes | blocked | after canary | after canary or `--trust-version` |
| mcp_lifecycle_tools | off | on, never bot-callable | on, never bot-callable |

`policy set` keys: `bots_scope` (`all` or a comma list), `confirm_destructive` (`typed|yes|none`), `debug_port`
(`off|ask-once|auto`), `port_ttl` (`45m`), `reply_wrapping` (`note|data|raw`), `action_log`, `mcp_lifecycle_tools`,
`transports.debug-port|ax|bot-shell|webhook` (`on|off`).

### Hard floors (`policy::floors`, checked first in `decide` and again at the transport edge)

1. `loopback_only(addr)`: the debug port is bound and reached on 127.0.0.1 only; `launch` restores and exits 4 if
   the app listens anywhere else.
2. `Action::AnswerApproval` is always refused; no approval/permission IPC is on the allowlist; `send`/`ask`/`stop`
   to a bot with a pending approval exit 3 without sending (a message would cancel the card).
3. `scrub()` / `looks_secret()`: secret-shaped words never reach proof lines or the action log; the log never holds
   message text.
4. `delete_exact(typed, id, name)`: exact id or exact (case-sensitive) name; over MCP `confirm` must repeat it; the
   main bot is never deleted.
5. The port file is 0600, the port number and websocket path are never printed or logged, `launch` arms a detached
   `_ttl-guard`, and every command restores an expired port first (`Lifecycle::enforce_ttl`). `watch` stops at the
   TTL unless `--extend`.
6. `ipc_allowed(method)`: only the methods in SPEC §6.1; payment, vault, secret, account, team, install, update and
   approval words are denied even if listed by mistake.
- Over an MCP server started with `--bot-callable`: stop, delete and lifecycle are never listed or callable.

## 4. Engine (what MCP and twin-as-library call)

```rust
pub struct Engine { pub policy, pub log: ActionLog, pub clock: SharedClock, pub caller, pub trust_version,
                    pub interrupted: Arc<AtomicBool>, pub deadline_ms: Option<u64>, pub asker: Asker, .. }
impl Engine {
    pub fn new(transports: Vec<Box<dyn Transport>>, policy: Policy, clock: SharedClock, caller: Caller) -> Self;
    pub fn capabilities(&mut self) -> TResult<Capabilities>;
    pub fn bots(&mut self) -> Outcome<Vec<Bot>>;
    pub fn resolve(bots: &[Bot], sel: &str) -> Result<Bot, Halt>;
    pub fn send(&mut self, bot: &str, msg: &str, files: &[PathBuf]) -> Outcome<SendResult>;
    pub fn ask(&mut self, bot: &str, msg: &str, files: &[PathBuf], o: &WaitOpts) -> Outcome<AskResult>;
    pub fn wait(&mut self, bot: &str, since: Option<&str>, o: &WaitOpts) -> Outcome<AskResult>;
    pub fn read(&mut self, bot: &str, since: Option<&str>, limit: usize) -> Outcome<Vec<Message>>;
    pub fn stop(&mut self, bot: &str, steer: Option<&str>) -> Outcome<StopResult>;
    pub fn watch(&mut self, bot: Option<&str>, for_s: Option<f64>, sink: &mut dyn FnMut(&WatchEvent)) -> Outcome<WatchSummary>;
}
pub struct Outcome<T> { pub value: Option<T>, pub exit: ExitCode, pub proof: Proof, pub hint: Option<String>, pub warnings: Vec<String> }
pub struct Proof { pub pass: bool, pub what: String, pub check: String, pub route: String }   // line() = PASS|FAIL: what; check
pub enum ExitCode { Done = 0, Other = 1, StillWorking = 2, NeedsUser = 3, Refused = 4, Unavailable = 5 }
pub struct WaitOpts { pub timeout_s: f64, pub settle_s: f64 }   // default 300 / 8
```

- Settle loop (only `bot_state` + `read`): needs-input returns at once (exit 3); idle for `settle_s` with at least
  one non-user message after our message → exit 0; timeout → exit 2. Polls every 400 ms.
- `stop`: refuses (exit 3) on a pending approval; a non-working bot is "nothing to stop"; otherwise the steer
  message, then idle for 1.2 s within 30 s (else exit 2).
- **Kept on purpose (reviewed in the P5 lane, unchanged):**
  - A message (`send`, `ask`, `stop`'s steer) to a bot whose newest entry is a pending approval exits 3 without
    sending. In 1.1.0 any new user message cancels the open card, so sending would silently answer it for the user
    (floor 2). The user answers the card in the app (or uses Grok's "Always allow"), then retries. Tests:
    `send_to_a_bot_awaiting_approval_is_refused_with_exit_3_and_sends_nothing`, `ask_needs_approval_is_exit_3_…`.
  - An approval card raised before our message is stale (chat-turn cards only; routine cards are never stale, §3a): our message already cancelled it, so the settle loop and
    `bots` ignore it (no exit 3) and wait for the reply instead. Only a card newer than our message counts.
    Test: `ask_routine_created_sees_the_event_and_ignores_the_stale_card`.
- Lifecycle: `Lifecycle { host: &mut dyn AppHost, home, clock }` with `status`, `launch(ttl_s)`, `restore`,
  `enforce_ttl`, `guard(opened_at_ms)`; `AppHost` is the seam (`MacHost` real; tests use a fake host).

## 3a. Additive after the freeze (P5 lane)

- **`status --json` → `local_tool_permission`** (optional): `"always" | "ask" | "never"`, the app's "Execution on
  Local Computer" setting, read through `getLocalToolPermission` (already on the floor-6 read allowlist). Read only:
  botport never sets it, and the setter is not allowlisted. On the real app it is read only when the debug port is
  open and was opened by botport (`status` never launches); if it cannot be read, or the answer is not one of the
  three values, the field is left out. Twin reads it to decide whether chat sync by routine is allowed. Code:
  `Transport::local_tool_permission` (default `Unsupported(Cap::LocalToolPermission)`), `Engine::local_tool_permission
  () -> Option<String>`, `parse::local_tool_permission`, `Status.local_tool_permission`. The fake reads
  `localToolPermission` from `roster.json` (`ask`); `FakeTransport::with_local_tool_permission(Option<Value>)`.
- **Routine approval cards are needs-input (exit 3).** Source: `control/ROUTINE-TRUTH.md`. A routine run's card does
  not set `awaitingUserResponse`, and the row never says "Waiting for you". Now also recognised:
  1. the row `aria-description` / `lastEntry.text` starting `Permission required:` (`parse::agent`, `parse::row`);
  2. a pending `local-tool-permission` card that is the newest bot entry while the working flags are set
     (routine cards can come with `isRunning` true);
  3. with a routine run `status:"running"`, `finishedAt:null` (`parse::routine_run_open`), any pending card in the
     transcript, even under a newer chat message: a chat message does **not** cancel a routine card, so it is never
     treated as stale. `parse::snapshot_with(bot, chat, routine_running)`; `parse::snapshot` = `routine_running` false.
  An `expired` card or a finished run clears it. New `NeedsSource::Routine` (not serialized; JSON unchanged). The
  debug-port transport gets signals 1 and 2 now; signal 3 is wired when P3 reads `listTeamBotAutomations` runs. The
  fake reads `routineRuns` (by bot id) from a snapshot fixture: `FakeTransport::load_routine_awaiting` /
  `load_snapshot(dir, file, clock)`, CLI `--scenario routine-approval`, fixture
  `tests/fixtures/app-1.1.0/transcript-routine-approval.json` (synthetic).

## 3b. P1 live acceptance fixes (app 1.1.0, TEST account; behaviour only, no interface change)

- The DevTools HTTP server keeps the socket open after `/json/version` and `/json/list`; bodies are read by
  `Content-Length` (`ws::http_get`). Before: every port command exited 5 (`os error 35`).
- Sidebar row labels carry status suffixes: `, Unread activity`, `, Needs attention` (chat-turn approval) and, on
  the pinned main row, `, Main Bot`. `parse::row` strips them (`parse::ROW_LABEL_SUFFIXES`). Before: `bots`
  failed its `dom-rows` check.
- A sent message first appears as `optimistic:<clientNonce>` and about 2 s later becomes `t<turn>u` with the same
  nonce. `send` (and so `ask`, `stop`) now returns the server key (`parse::confirmed_key`), so `wait --since KEY`
  and ask's "messages after our message" work.
- After a launch (the command or an automatic one) the first read waits up to 20 s for the app's first render
  (`transport::wait_until_rendered`); the sidebar has 0 rows for about 0.5 s after the port opens.
- `watch` message events carry the message over the debug port too (`ReadOpen`: only the bot open on screen;
  watch never switches the screen). Tests: `FakeTransport::read_open_only(bot)`.

## 3c. P3: routines, usage, files (additive; app 1.1.0, live on the TEST account 16 Nov 2026)

CLI (all take the common options; human output ends with the PASS/FAIL line, `--json` one object):

```
botport grok usage
botport grok routines <bot> [list]
botport grok routines <bot> enable|disable <routine>
botport grok routines <bot> add --name N --schedule "m h dom mon dow" [--tz TZ] --prompt P [--disabled] [--timeout 300]
botport grok routines <bot> remove <routine> [--yes]          exact id, slug or name only (floor 4)
botport grok routines <bot> run <routine>                     see "run" below
botport grok files <bot> [list]
botport grok files <bot> fetch [NAME|KEY] [--out DIR]          a file the bot SENT in chat (newest when no NAME)
botport grok files <bot> download [NAME...] [--out DIR]        the bot's agent files (all when no NAME)
botport grok files <bot> upload PATH...                         25 MB each
botport grok files <bot> delete NAME [--yes]                    exact name or path only (floor 4)
botport grok launch --for DUR [--when-idle MIN]                 issue 02, see §3d
```

| Command | JSON (besides exit, proof) | PASS checked by (`proof.route`) |
|---|---|---|
| usage | `percent_used, resets_at, plan, on_demand_cents, on_demand_limit_cents, available` | `self-check` |
| routines (list) | `routines: [Routine]` | `self-check` |
| routines enable/disable | `bot, action, routine, via:"ipc", messages:[]` (`RoutineChange`) | a fresh list shows `enabled` (`routines-list`) |
| routines add | `RoutineChange` (`via:"bot"`, `messages` = the bot's replies) | a fresh list shows the name with the asked schedule (`routines-list`) |
| routines remove | `RoutineChange` (`routine:null`, `via:"ui"\|"bot"`) | absent from a fresh list (`routines-list`) |
| routines run | `RoutineChange` | a new run in a fresh list (`routines-list`) |
| files (list) | `files: [FileEntry]` | two reads agree (`self-check`) |
| files fetch | `SavedFile` (`name, saved_to, bytes, sha256, app_sha256`) | disk sha256 = the app's sha256 of the bytes it loaded (`app-preview`) |
| files download | `saved: [SavedFile]` | disk sha256 = app's (when given) and size = the list's (`files-list`) |
| files upload | `files: [FileEntry]` | the list shows each with the same size (`files-list`) |
| files delete | `file: FileEntry` | absent from the list (`files-list`) |

- `Routine` gains `running` (a run with `status:"running"`, no finish) and `runs` (recorded run count). `FileEntry`
  is unchanged. New: `SavedFile`, `RoutineChange` (`model.rs`). `Message.file` is now filled for a bot-sent file
  (`send-message` / `message.type:"attachment"`): `{name: file_name, path: url}` (`url` is the
  `file:///home/box/…/assets/<hash>.<ext>` path on the bot's computer).
- Exit codes as §1. Not found / ambiguous: 1. Non-exact name for remove/delete: 4 (`floor 4`). `confirm_destructive`
  applies to routine remove and file delete as to bot delete. add/remove-via-bot propagate the wait's 2/3.
- **Download never overwrites**: names are reduced to one safe path component; a taken name gets ` (1)`, ` (2)`;
  the file is created with `create_new`, synced, read back and hashed; a file that fails its check is removed again
  (exit 1). Bytes move in 1 MiB base64 chunks over the port (3.2 MB fetched live in 5 s; 30 MB offline).
- **fetch vs download (1.1.0 ground truth).** `readAttachmentBytes/Text` fail over the port for every file
  (`attachments/read-abandoned: … a view of a departed account`), both for bot-sent files and for agent files.
  `fetch` therefore tries the direct read first (a later version may allow it) and then the app's own preview: it
  scrolls the virtualized transcript to the file's row, clicks the row's **"Open <name>" button** (the file card's
  open control; never an approval card), reads the bytes the preview loaded (`{status:"loaded", bytes:{status:
  "ready", bytes: Uint8Array}}`), and closes it with "Back to chat". Files with no preview (e.g. `.bin`: the row has
  only "Save", a native dialog) exit 1 naming that; ask the bot to send `.json`/`.txt`. `download` (agent files) has
  no preview route yet: on 1.1.0 it exits 1 with the app's `read-abandoned` error. Bot-sent files are **not** in
  `listGrokBotAgentFiles`.
- **Routines on 1.1.0**: add works by asking the bot (it emits `automation-changed`); `--disabled` then toggles it
  off by the direct call. **run is not possible**: the bot answers that it cannot fire a routine on demand, so
  `routines run` exits 1 ("no new run in the routine list") — kept, so a later version that can run is detected.
  remove: the debug-port transport has no menu route yet (`Cap::RoutinesRemove` not offered), so the engine
  disables the routine by the direct call and asks the bot to delete it; PASS by a fresh list.
- **Routines audit** (`Engine::routines_audit`, `routines audit [--used-by NAME]... [--json]`): read-only, lists
  only (`listTeamBotAutomations` per bot in scope), 0 bot turns. Per routine: schedule, runs a week from the cron
  (an average for day-of-month/month limits; `?` when unreadable), estimated turns a week (each run is one paid turn,
  also when it finds nothing to do; 0 when disabled), last run and run count. An enabled routine whose name, slug or
  prompt names no mirror pair (`$XDG_CONFIG_HOME/botport/mirror/<pair>.toml`) and no `--used-by` name is flagged.
- **Usage check** (`Engine::usage_gate`): before send/ask (and so routine add/run/remove-via-bot), read once per
  engine. `warn` presets add a `warnings` entry at the threshold or when `available` is false; `refuse` (careful)
  exits 4 `refused by usage_check`. A failed read adds a warning; a transport without usage skips it. The summary's
  `dashboardUrl` (holds the account id) is never read.
- **Turn ledger and budget** (`Engine::budget_gate`): every paid call that got its message in (send, ask, and the
  asks inside routine add/run/remove-via-bot) appends `{at_ms, cmd, caller, bot_id}` to `~/.botport/grok-turns.jsonl`
  (0600; kept under `--no-log`; caller from `--tag`, `BOTPORT_CALLER`, else `cli`/`mcp`). Reads, routine lists and
  `usage` are never recorded. With `usage_budget_week` set, a paid call warns from `usage_warn_pct` of it and, once
  the last 7 days hold the budget, exits 4 `refused by usage_hard_cap` (a warning when `usage_hard_cap off`).
  `usage` prints `N turns by botport this week, budget M`.
- **Needs-input signal 3** is wired on the debug port: when the open chat holds a pending approval card that the
  flags do not explain, `listTeamBotAutomations` is read and an open run makes it needs-input (`snapshot_with`).
- Floor 6 allowlist unchanged (all P3 calls were already listed). `CdpTransport` errors now carry the app's thrown
  message (first line, scrubbed, 200 chars) instead of "a read snippet threw".
- Transport (additive default methods): `file_read_checked(id, &FileEntry) -> (bytes, Option<app_sha256>)`,
  `attachment_read(id, key, &FileRef) -> (bytes, Option<app_sha256>)`. `AppHost::idle_s()` (default None).
- Fake: `with_usage(pct, available)`, `without_usage()`, `without_ui_remove()`, `bot_ignores_routine_requests()`,
  `with_file(bot, name, bytes)`, `with_attachment(bot, name, bytes)`, `with_bad_app_digest()`; it reads
  `automations.json`, `files.json`, `usage.json` and plays the bot for the routine add/run/remove messages
  (`routines::add_prompt` etc.). New `RecordedWrite` variants: `RoutineEnabled, RoutineRemoved, FileUploaded,
  FileDeleted`.
- MCP: `grok_usage {}`, `grok_routines {bot, action: list|add|enable|disable|run, routine?, name?, schedule?, tz?,
  prompt?, disabled?, timeout_s?}`, `grok_files {bot, action: list|download|fetch|upload, names?, paths?, out?,
  name?}`, `grok_routine_remove {bot, routine, confirm}`, `grok_file_delete {bot, name, confirm}` (confirm must repeat
  the exact name). On `--bot-callable`: no `grok_routine_remove`, `grok_file_delete`, `grok_stop`; `grok_routines`
  and `grok_files` offer only `list` (a bot must not upload local files into itself, write files here, or change
  routines). 12 tools, 9 bot-callable.

## 3d. Short port windows for scheduled work (issue 02, additive)

- `launch --for DUR` = `launch --port-ttl DUR`, plus: a botport port that is open (same pid) and lasts at least DUR
  is **reused, no restart** (`PASS: port already open until HH:MM; reused; no restart`, route `port-file`, JSON adds
  `reused:true`). `--when-idle MIN`: restart only when the Mac has had no keyboard/mouse input for MIN minutes
  (`ioreg` HIDIdleTime); otherwise **exit 2** (`FAIL: launch; the Mac was used N min ago …`, hint "try again later").
  With the port closed the app's turn state cannot be read without a restart, so the idle test is the Mac's input
  idle time; the job ends with `restore` (or the TTL guard does it).
- Crash safety: `launch` writes `~/.botport/grok-launching` before starting the app with the port and removes it once
  the port file is saved. `Lifecycle::enforce_ttl_why() -> Option<Leftover{Expired|Orphan}>` (run by every command)
  restores an expired window, and also a launch that died half way (marker, no port file). A port opened by hand
  (no marker) is left alone. `launch --for` names what it restored in a `warnings` entry.
- After a launch with `--for`: the sign-in screen → exit 5 `Grok Bot is signed out …; restored`; an untested version →
  exit 4 `… not a tested version (unknown_version_writes) …; restored`.

## 3e. 0.3.0 review fixes (REVIEW-engine-030, additive)

Every entry adds to the interface; nothing existing changes shape. Tests: `crates/botport-grok/tests/review_030.rs`.

- **H1 (floor 5 over MCP).** `mcp::serve_checked(make, status, fresh, input, out, bot_callable)` and
  `mcp::engine_fresh(&Engine) -> bool` (new; `mcp::serve` is unchanged and calls `serve_checked` with a `fresh` that
  always says yes). Before every tool call except `grok_status`, the cached engine is dropped when its
  `deadline_ms` has passed or `fresh` says no; the next call rebuilds it through `make` (so `build_engine`'s
  `enforce_ttl`, AutoLaunch policy and bot-callable refusal apply again). `botport grok mcp` passes a `fresh` that
  runs `enforce_ttl` and `cli::port_file_matches(port_file, main_pid, deadline_ms) -> bool` (new). Behaviour:
  `ask`/`wait` (CLI and MCP) never wait past `Engine::deadline_ms`; hitting it ends with exit 2 and
  `FAIL: wait; port TTL reached (--extend to go on)`, as `watch` already did.
- **H2 (fetch reads only the app's asset URL).** `policy::floors::attachment_url_ok(path, bot_id) -> bool` (new):
  true only for `file:///…/agent-data/agents/<bot id>/assets/<hash-like name>`. The debug port calls
  `readAttachmentBytes` for an attachment only when it passes; otherwise `files fetch` goes straight to the app's
  preview route. The fake transport applies the same check and gains `with_attachment_url(bot, name, url, direct,
  preview)` (new builder).
- **M1 (guard detached).** `lifecycle::detach(&mut Command) -> &mut Command` (new): `setsid` before exec. `MacHost::
  arm_guard` spawns `_ttl-guard` through it, so the guard has its own session and process group and survives
  Ctrl-C, SIGHUP and a process-group kill of the command that launched it.
- **M2 (hit-tested clicks, floor 2).** `transport::cdp::click_point(&Value) -> TResult<Option<(f64, f64)>>` (new).
  The two coordinate clicks (sidebar row, the preview's "Open <name>" button) read `document.elementFromPoint` in
  the same evaluate that measures the point and again right before `mousePressed`; the press happens only when the
  element under the point is the target (or inside it) and no approval control (an Allow/Approve/Deny button, an
  `alertdialog`) is under it. Otherwise `TransportError::Floor` and no press.
- **M3 (wrapping).** Under `reply_wrapping = data | note`, MCP now also wraps `grok_bots`, `grok_usage`,
  `grok_routines` (every action, `list` included), `grok_files` (every action) and `grok_file_delete`: the result
  moves under `untrusted_bot_content` (plus `note`). `raw` (the `fast` default) is unchanged.
- **M4 (MCP local paths).** `mcp::confine(root, path, existing_file) -> Result<PathBuf, String>` (new). Over MCP,
  `grok_files` `upload` `paths` and `download`/`fetch` `out` must stay inside the server's working folder (resolved
  with symlinks followed); `..`, dotfiles and dot-folders, and symlinked upload files are refused (JSON-RPC error
  -32602, as other bad arguments). `files upload` (CLI and engine) refuses a symlink (exit 4), and the debug port
  checks again before reading the bytes.
- **M5 (crash-safe downloads).** `files::save_new_checked(dir, name, bytes, check) -> io::Result<(PathBuf, u64,
  String)>` and `files::partial_name(name)` (new; `save_new` keeps its signature and calls it). `files download`
  and `fetch` write to `.<name>.botport-partial-<pid>` (0600), sync, read back, hash, run the sha256/size check and
  only then hard-link the file under its final name (`link` never replaces; ` (n)` as before) and remove the
  temporary name. A failed check or a crash never leaves a truncated file under the final name. On a file system
  without hard links the checked bytes are copied into a `create_new` file instead.
- **M6 (exact delete).** `files delete` is refused (exit 4, floor 4) when more than one listed file has the chosen
  file's name, since `deleteGrokBotAgentFile` takes a name. The after-check also fails (exit 1) if any other file
  left the list. Fake transport: `with_file_at(bot, name, path, bytes)` (new builder).
- **L1, L2 (parsing).** `lifecycle::MAX_PORT_TTL_S` = 24 h (new). `policy set port_ttl`, `launch --for` and
  `--port-ttl` refuse more; `Lifecycle::launch` clamps a larger TTL (an old policy file) to it; `PortFile::expires_ms`
  saturates. `util::parse_duration_s` refuses `inf`, `nan` and values over 1e9 s. `util::parse_rfc3339` refuses a
  non-ASCII offset instead of panicking.
- **L3, L4 (modes).** Downloaded and fetched files, and `files::create_unique`, are created 0600.
  `util::append_private` sets 0600 on a file that already existed wider. `util::private_dir` chmods 0700 only a
  folder it creates; an existing folder (for example `BOTPORT_HOME=$HOME`) keeps its mode.
- **L5 (websocket handshake).** New: `transport::ws::is_upgrade_status(line)`, `transport::ws::valid_ws_path(path)`,
  `transport::cdp::ws_target_path(url, prefix) -> TResult<&str>`. The page target URL must be
  `ws://127.0.0.1:<port>/devtools/page/…` exactly (so `:9222` no longer matches `:92220`; else
  `TransportError::Floor`); `Ws::connect` refuses a path with spaces or control characters; the upgrade needs the
  status line `HTTP/1.1 101`; an HTTP body without `Content-Length` is capped at 16 MB.
- **L6 (unverifiable downloads).** A `files download` where some file came with no app sha256 and a listed size of 0
  still saves it but reports `route: "self-check"` with the check `self-check only: the app gave no sha256 and the
  file list no size for <names>` instead of `files-list`. Fake transport: `without_app_digest()` (new builder).
- **L7 (one process at a time).** New: `util::lock_file(path) -> io::Result<File>` (exclusive advisory lock, 0600),
  `lifecycle::home_lock(home)` (`~/.botport/grok.lock`), `Engine::page_lock: Option<PathBuf>`. `launch`, `restore`
  and `enforce_ttl_why` hold the lock, so a launch in another process (between `start` and saving its port file)
  is no longer restored as an orphan. With `page_lock` set (the CLI and MCP set it for the debug port), every
  transport operation holds the lock, so an MCP server and a CLI command never interleave clicks.
- **L8 (floor 6).** `subscribe` is removed from `floors::IPC_READ`: no code calls it and its generic name cannot be
  judged by the deny-word net. This narrows the allowlist (a floor), it removes nothing a caller used.
- **L9 (docs).** §5 now says to pair `--bot-callable` with a `bots_scope` allowlist, since `grok_ask`/`grok_send`
  stay bot-callable by design.

## 4a. Versions and canary (P5, additive)

- **Tested-versions table** (`version::TESTED`, `version::lookup`, `version::is_tested`):

  | Version | Reads | Writes | Notes |
  |---|---|---|---|
  | 1.1.0 | tested | tested | fixtures `tests/fixtures/app-1.1.0`; P1 live suite |

  A row is added only after a live canary and the live suite pass on a TEST account. 1.0.1 (seen on the mini) is
  not in the table: it runs as an unknown version.
- **Version probe on connect:** the debug-port transport reads the `GrokBot/x.y.z` User-Agent from `/json/version`
  before attaching; `Capabilities { app_version, tested }` carry it; no version found means `"unknown"` (untested).
- **Unknown version:** reads run and carry a `warnings` entry naming the version. Writes (send, ask, stop) exit 4
  naming `unknown_version_writes` until a canary has passed on that version **and** that transport (a fake-transport
  canary never unlocks the debug port). careful: always blocked; balanced: after the canary; fast: after the canary
  or with `--trust-version`. A write let through carries a warning naming the canary time or the flag.
- **`botport grok canary`** (`canary::run(&mut Engine, dir) -> Outcome<CanaryReport>`, `canary::passed(dir, version,
  transport) -> Option<rfc3339>`):
  1. `port`: connecting is the first check (`launch` under the preset's `debug_port` rule if closed), then
     `Transport::port_check` — `/json/version` still answers with the same version, exactly one page target, and a
     trivial `Runtime.evaluate` returns. This is the vendor DevTools gate check; if it fails nothing else runs (exit 5).
  2. Read-only probes: `status` (capabilities + version), `bots` (props vs sidebar rows), `state` (one
     `bot_state` per bot: ask/wait/watch), `read` (the main bot, 5 messages; skipped, not failed, if policy refuses),
     `send` and `stop` (capability present; not exercised — `--write` needs P4's create/delete and exits 1 for now).
  3. Report `~/.botport/canary/canary-<version>-<YYYYMMDDTHHMMSSZ>.json`, 0600, counts only (no bot names, ids or
     message text). A failed connect is filed too.
  - PASS: `PASS: canary 1.1.0 read 7/7; report canary/canary-1.1.0-…json` (exit 0). A failing probe: exit 1 naming
    the step and the field; port: exit 5.
  - `--transport fake --fixtures DIR` runs it fully offline against the fixtures (transport `fake` in the report).
- Engine additions: `pub canary_dir: Option<PathBuf>` (the CLI sets `~/.botport/canary`; None in a library engine
  means no canary can unlock), `pub fn transport_name()`, `pub fn port_check()`.

## 5. MCP (`botport grok mcp`, stdio JSON-RPC, one message per line)

Tools: `grok_status`, `grok_bots`, `grok_ask {bot, message, timeout_s?, settle_s?}`, `grok_send {bot, message}`,
`grok_wait {bot, since?, timeout_s?, settle_s?}`, `grok_read {bot, limit?, since?}`, `grok_stop {bot, steer?}`
(not with `--bot-callable`). A `--bot-callable` server still offers `grok_ask` and `grok_send`, so any bot that
reaches it can drive every bot in scope: pair `--bot-callable` with a `bots_scope` allowlist
(`botport grok policy set bots_scope <bot>,<bot>`) naming only the bots it may talk to. Results: `structuredContent` = the CLI's JSON object, `content[0].text` = the same
serialized; `isError` is false for exit 0/2/3.

## 6. Not in P1 (open for the next lanes)

Routines, usage, files: done in P3 (§3c), except attachments on `send --file` and a menu route for routine remove;
create/rename/delete (P4); `canary --write`
(needs P4) and live canary runs on new versions (P5 follow-up); `ax`, `bot-shell` and `webhook` transports. `--transport ax|bot-shell|webhook`
exits 5 naming that it is not built yet.

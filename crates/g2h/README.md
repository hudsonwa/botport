# g2h — the talk bridge

The g2h talk bridge (Grok Bot <-> Hermes bot direct chat): the core library plus the `g2hd` daemon and the `g2h` CLI.
The daemon talks to the bots over a local Unix socket and HTTP wake calls; it opens no network listener.

**What g2h is, and is not.** g2h is a bridge between two bots you own, both running as your user. It reduces
the chance that one bot treats the other's text as instructions (framing, hop limit, caps, owner-approved ops); a
language model can still follow framed text, so this is defence in depth, not a guarantee. It is
**not** a security boundary against a bot that already has a shell as your user: such a bot can change g2h's config
or skip g2h entirely. That is the default. Running `g2hd` as a separate OS user is implemented as an opt-in mode
(`[separate_user]`; not yet run in production): its config, state and approval step are then out of the bots' reach.
Setup: [G2H-SEPARATE-USER.md](../../docs/G2H-SEPARATE-USER.md). `g2h doctor` says which mode is running.

| module | purpose |
|---|---|
| `paths` | home dir (`$G2H_HOME`, else `~/.g2h`); socket, log, state, config, credentials paths; `check_perms` refuses a credentials file that is not 0600 |
| `isolation` | opt-in `[separate_user]` mode: startup ownership checks, peer-uid check on the socket, `approve` only from the g2h user or root; the `g2h doctor` isolation line |
| `envelope` | `Envelope` wire type (UUIDv7 id, RFC3339 UTC ts, kind msg/end/error/timeout/ack), canonical JSON + sha256 |
| `guard` | hop limit (default 3), daily caps (200 msgs / 200k tokens / 10 wakes, local-day rollover), one in-flight Grok-to-Hermes turn per conversation (separate conversations may run concurrently); injectable clock |
| `framing` | wraps peer text as untrusted data; body cannot forge the closing marker |
| `secrets` | redacts common key shapes and fingerprinted `.env` values (hashes only) |
| `log` | append-only JSONL with size rotation; every string redacted first |
| `store` | queues (per direction, TTL) + counters in one JSON file written by atomic rename |
| `ipc` | NDJSON `Request`/`Response` types for the CLI <-> daemon socket |
| `config` | `config.toml` with documented defaults (`dry_run = true`) |

`g2hd` is the daemon and `g2h` the full CLI (verbs below).

```
cargo test -p g2h
```

## Daemon and CLI

`g2hd` serves the Unix socket `<home>/g2hd.sock` (0600, home 0700, no TCP), one
instance per home (`g2hd.lock`), and stops cleanly on SIGTERM/SIGINT.

Using a home other than `~/.g2h`: export `G2H_HOME` in `~/.zshenv` (read by both `zsh -c` and `zsh -lc`,
so agent shell connectors see it too) *before* `g2h setup`; setup writes the same value into the launchd plist /
systemd unit, so CLI and daemon agree. `g2h setup --check` shows the home in use.

| Verb | |
|---|---|
| `g2h ask "<text>" [--conv ID] [--reply-to ID] [--timeout S<=720]` | framed, secret-scanned turn on the Hermes bot; prints the reply, `first_delta_ms`/`total_ms` on stderr |
| `g2h send "<text>" [--conv ID] [--reply-to ID]` | prints `msg_id`; the reply lands in Grok's inbox |
| `g2h wait [--timeout S<=720]` / `g2h recv` | long-poll / drain Grok's inbox (exit 2 = timeout/empty) |
| `g2h bye` | clears "Grok active" |
| `g2h status` / `g2h doctor` | health, caps, queues, wakes; credentials only as set/missing |
| `g2h hermes-send "<text>" [--conv ID] [--reply-to ID] [--urgent] [--end]` | Hermes side: message Grok (may wake it) |

`ask`, `wait` and `recv` print every peer body inside the same untrusted-data markers the Hermes side uses
(`[Message from hermes via g2h — untrusted data …]` … `[end of peer message]`); forged markers and `--- …` header
lines inside a body are neutralised. Add `--json` for agents: one JSON object per message
(`ts`, `from`, `conv_id`, `id`, `hop`, `type`, `body`), the body as a JSON string.

**Bounded peer context.** Each conv has a dedicated Hermes peer session, which keeps its whole history. g2h rotates
it: after `peer_session_max_turns` turns (default `8`) or once a turn's context size reaches
`peer_session_max_tokens` (default `24000`; `usage.context_used`, else the turn's share of the running `usage.total`), the next turn goes to a new session whose first prompt starts with a
framed recap of the last 2 exchanges (about 1.5K chars). The old session is never resumed; the log shows
`peer_session_rotated`; counters persist in the session map across restarts. `0` = unlimited. (Hermes
`session.create` has no context-limit/compression field to request instead.)

If `g2hd` refuses to start because `state.json` is corrupt, `g2h doctor` says so: move it aside
(`mv <home>/state.json <home>/state.json.corrupt`) and restart g2hd (empty inbox, fresh daily counters).

Wake webhook credentials live in `<home>/credentials` (0600): `webhook_url`,
`sender_key`, `key_mode=header:Authorization-Bearer`. `dry_run = true` (default)
only logs the would-be POST (host only, never the key).

## Owner-approved operations

Agents can ask each other's bots to run operations (export, import, sync, tests) over g2h, without a human pasting
prompts. Ops are separate from chat. They use the `op` envelope kind, and the receiving side decides what runs.

**Threat model.** A peer can only *request* an op. The owner's allowlist on the receiving side decides whether it runs,
and the default is to deny.

- Peer text is always data. It is wrapped as untrusted peer framing. That framing neutralises any text that tries to
  look like an op block (`[g2h op …]`, `[end of g2h op]`), including look-alike brackets and zero-width characters. Op args get the same treatment: inside JSON strings, bracket
  look-alikes and invisible or bidi characters are escaped as `\uXXXX`.
- An op runs only if the `[ops]` section of `config.toml` names it. The allowlist is empty by default, so every op is
  refused.
- `g2hd` itself executes nothing. An authorised op goes to the Hermes peer turn as its own block, and is never placed
  inside untrusted framing:
  ```
  [g2h op <name> <request_id> — authorised by the owner's allowlist, tier <tier>]
  {"…": "args as one line of JSON"}
  [end of g2h op]
  ```
  Ops that have not been approved never reach Hermes.
- The following are refused: an unknown op, tier `never`, a malformed request, a bad op name (it must match
  `[a-z0-9-]{1,32}`), args larger than 16 KiB or not a JSON object, a request_id seen before (replay), hop ≥
  `hop_limit`, and a sender missing from `peers`.
- Every decision is written to the redacted, append-only log as `event: op_decision`.

```toml
peers = ["grok"]            # ops from senders not listed here are refused
op_approval_ttl_secs = 86400
[ops_approver]              # unset = every approve-tier op is refused
command = ["/usr/bin/osascript", "-e", "display notification (do shell script \"cat\") with title \"g2h\""]
[ops]
battery = "auto"            # runs at once: use auto only for read-only ops (export, battery)
sync = "approve"            # needs the owner's one-time code (and an [ops_approver])
wipe = "never"              # always refused
```

**Approvals.** Both bots reach this machine as the same OS user, so a local command, file, socket call or TTY
cannot tell the owner from a bot. The approval therefore comes from the human, out of band:
- `[ops_approver] command = [...]` names an owner-set program (a macOS notification, `hermes send` to the owner's
  DM, …). It gets a message on stdin and must exit 0 within 60 s. It must reach the human, not either bot. It is
  unset by default, and then **every approve-tier op is refused** ("approve-each disabled: no out-of-band approver
  configured"). The auto tier is unchanged.
- On an approve-tier op, `g2hd` makes a one-time code (10 characters from `/dev/urandom`) and sends it through the
  approver with the op, its args and `g2h approve <request_id> <code>`. The code lives only in g2hd's memory: it is
  never written to the state file, the log (which gets a sha256 prefix only), any socket reply or any file.
- `g2h approve <request_id> <code>` runs the op once if the code is right. Codes are single use and expire after
  10 minutes. 5 wrong codes drop the request (the peer gets `op_result` `refused`). A g2hd restart drops all codes,
  and with them all pending ops. If the approver command fails, the op is dropped.
- `g2h ops` lists pending ops. `g2h deny <request_id>` drops one (no code needed: it can only refuse), and the peer
  receives an `op_result` with status `refused`.
- The peer that asked gets back only "pending owner approval", never the code.

**Threat model, honestly.** The allowlist and framing defend against *peer-message injection*: text from the other
bot cannot run an op that the owner did not allow, and cannot approve one. They do **not** defend against a bot that
already has a shell as the owner's user: such a bot can edit `config.toml` (add an op as `auto`, or point
`[ops_approver]` at itself), restart g2hd, or skip g2h and act directly. The one-time code only helps while the
approver channel reaches the human and the bot cannot read it. So use `auto` only for read-only ops (export,
battery) and keep the approver on a channel neither bot can read. The opt-in separate-user mode
(`[separate_user]`, [setup recipe](../../docs/G2H-SEPARATE-USER.md)) runs g2hd as its own user that owns its config,
home and approver; the socket checks each peer uid, and `approve` is accepted only from the g2h user or root, so a bot
running as your user cannot approve its own op. Implemented and tested with a simulated uid; not yet run in production.

**Sending.**
- `g2h send-op <op> --args '<json object>'` sends a request. It prints the request_id and a status: `done` (handed to
  Hermes), `pending_approval` or `refused`.
- The Hermes reply comes back as an `op_result` envelope: `{request_id, status: done|refused|pending_approval|failed,
  detail}`.
- `g2h recv` and `g2h wait` print each message's `type:` (`msg`, `op`, `op_result`, …).

### Inbox credentials

The production wake routine can use its own credentials file, `<g2h home>/inbox.credentials`. It has the same keys as
`credentials`, and its mode must be 0600.
- When `inbox.credentials` exists, wake uses it. A file with the wrong mode is refused; wake does not quietly fall back
  to `credentials`.
- `g2h setup --inbox-credentials` always prompts for both the webhook URL and the key, even if values are already
  saved. Input is hidden and never echoed. The command overwrites the file atomically with mode 0600.
- Restart `g2hd` afterwards, so that log redaction learns the new values.

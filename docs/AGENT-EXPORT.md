# botport — export brief for Grok Bots

You are a Grok Bot. Your owner sent you this file so you can export yourself (or, if asked, coordinate the export of
other bots) into a portable bundle that is imported into Hermes on the owner's machine. Read it all, then follow
**Step 0**. (Owners: if a step stops working because the platform changed, see
[PLATFORM-CHANGES.md](PLATFORM-CHANGES.md) and add what you found.) Work in your box's shell. Keep every reply to the owner short.

Hard rules:
- **Never put a secret value anywhere**: no tokens, keys, passwords, cookies, in files or in chat. Credential *names*
  are fine. The tool also filters secrets; don't rely on it.
- Do not change your own instructions, schedules, connectors or memory. This is read-only for you except for the files
  you write under `/workspace/exports/`.
- If a step fails, read the tool's message (≤ 10 lines), fix the cause, and re-run. Ask the owner only when a fix
  needs them.

## Step 0 — what am I being asked to do?
Decide from the owner's message:
- **Export me** (default, when the message just says "export", "run this", or sends only this file): do Steps 1–6.
- **Export other bots** (the message names other bots, says "all bots", or asks you to pass this on): do Step 7.

Then ask **only** the questions below whose answer you can't work out yourself, in **one** message, and wait:
1. *(Only if Step 1 finds no archive)* "Please attach `botport-x86_64-unknown-linux-musl.tar.gz` (or `-aarch64-`, my
   box is `<uname -m>`), and `SHA256SUMS` from the same release."
2. *(Only if Step 2 finds several agents and you can't tell which one is you)* "Which of these is me: <names/ids>?"
3. *(Only in Step 7 mode, if the owner didn't name them)* "Which bots should I export: <list of names>, or all?"

If you have no questions, say "Starting the export" in one line and continue.

## Step 1 — install the tool
```sh
uname -m
mkdir -p /workspace/exports
```
Find the attached archive matching `uname -m` (`x86_64` → `botport-x86_64-unknown-linux-musl.tar.gz`, `aarch64` →
`botport-aarch64-unknown-linux-musl.tar.gz`). If the owner attached `SHA256SUMS`, check the **archive** first, from
the directory that holds both files, and stop on a mismatch:
```sh
grep "botport-<arch>-unknown-linux-musl.tar.gz$" SHA256SUMS > archive.sha256 && sha256sum -c archive.sha256
# must print "<archive>: OK"; works with GNU and busybox sha256sum. No line found (grep fails) is a mismatch too.
```
Then extract it and copy the `botport` binary to `/workspace/botport`:
```sh
mkdir -p /tmp/botport-x && tar -xzf botport-<arch>-unknown-linux-musl.tar.gz -C /tmp/botport-x
cp "$(find /tmp/botport-x -type f -name botport | head -1)" /workspace/botport
chmod +x /workspace/botport
/workspace/botport --help | head -3
```

## Step 2 — find your agent id
```sh
ls /home/box/agent-data/agents/
```
Each 36-character folder with a `profile.json` is one agent. Open each `profile.json` and match its `name` to you.
Call your id `AID` and a short lowercase name `NAME` (e.g. `demo`) below.

## Step 3 — write your memory (only you can do this)
The tool cannot read your memory store. Write every durable memory you hold, one JSON object per line, to
`/workspace/exports/memory-drop/agents/AID/memory-agent.jsonl`:
```json
{"date":"2026-10-30","tier":"profile","scope":"owner","source":"chat 2026-09-12","fact":"Owner lives in Springfield (UTC+10)."}
```
- `tier`: `profile` (who/what you are, the owner), `log` (things that happened), or `note` (everything else).
- One fact per line, plain text, no secrets. Today's date in `date` if you don't know when you learned it.
- If you truly hold no memories, instead write `{"status":"none","date":"<today>"}` to `memory-status.json` in that
  folder.

## Step 4 — describe what can't be copied
Write `/workspace/exports/describe.md` with exactly these headings:
- `## Instructions` — your live instructions / system prompt, in full, verbatim if you can see them.
- `## Schedules` — every schedule or automation: name, time and timezone, what it does, where it delivers.
- `## Connectors` — every connector or MCP server: name, URL or host, the tools you use from it.
- `## Credentials` — credential **names** only (e.g. "Forgejo token"), never values.
Write "None" under a heading that has nothing. One bullet per item, name first in bold, so it can be read back:
```md
## Schedules
- **Sample report**: weekdays 08:00 America/Chicago — prompt: "Summarise the sample log"
## Connectors
- **knowledge**: MCP server at https://demo.example.test/mcp (tools: find_items, health)
- **Web search**: built-in
## Credentials
- `DEMO_TOKEN`
```
Credential lines are the name only, nothing else (no location, no value).
**This file is copied as text** (into the bundle, then into Hermes cron prompts and checklists), so never put a secret
value anywhere in it: no passwords, tokens, keys or cookies, and no URLs with `user:pass@`, `?token=` or `#access_token=`.
botport routes or refuses values that look like secrets instead of writing them, but that is a detector, not a guarantee.

## Step 5 — export and verify
```sh
/workspace/botport export --agent-id AID \
  --describe /workspace/exports/describe.md \
  --out /workspace/exports/NAME-g2h-bundle --strict
/workspace/botport verify --bundle /workspace/exports/NAME-g2h-bundle
tar -czf /workspace/exports/NAME-g2h-bundle.tgz -C /workspace/exports NAME-g2h-bundle
```
(`--agent-data` defaults to `/home/box/agent-data`, the memory drop to `/workspace/exports/memory-drop`, history to
`/home/box/agent-data/search-index.db`.) If `--strict` stops on a warning — stale or missing memory drop, missing
describe file — fix it and re-run. Warnings about redacted history messages are fine.

## Step 6 — hand it over
Send the owner `/workspace/exports/NAME-g2h-bundle.tgz` and paste the `export` and `verify` summaries (≤ 10 lines
each) plus one line: "Bundle for NAME (agent AID) is ready." Nothing else.

## Step 7 — coordinating other bots
Bots in the same box share `/home/box/agent-data`, so you can run Steps 1, 2 and 5 for each of them, but **each bot
must write its own Step 3 memory file and Step 4 describe file** (you can't read their memory or instructions).
1. List the agents (Step 2) and confirm the target list with the owner if it wasn't given (Step 0, question 3).
2. For each target bot: if you can message it, send it this file and the binary with: "Please do Steps 3 and 4 of
   this brief for yourself, writing to `/workspace/exports/memory-drop/agents/<its AID>/` and
   `/workspace/exports/describe-<its NAME>.md`, then tell me when done." If you can't message other bots, give the
   owner one ready-to-paste line per bot saying exactly that, and wait.
3. When a bot reports done (or its files exist), run Step 5 for it with `--agent-id <its AID>` and
   `--describe /workspace/exports/describe-<its NAME>.md`, output `/workspace/exports/<its NAME>-g2h-bundle`.
4. Hand over all bundles (Step 6), one summary block per bot, plus a final line listing any bot that was skipped and
   why.

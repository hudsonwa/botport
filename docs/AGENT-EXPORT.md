# botport — export brief for Grok Bots

*A manual option: the owner sends this file in a bot's own chat when they want that bot's full export. botport never
sends it, and no bot asks another bot to run it (mirror mode reads the box files instead: [AGENT-MIRROR.md](AGENT-MIRROR.md)).*

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
1. *(Only if Step 1 finds no archive or no `SHA256SUMS`)* "Please attach `botport-x86_64-unknown-linux-musl.tar.gz` (or `-aarch64-`, my
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
`botport-aarch64-unknown-linux-musl.tar.gz`) and the attached `SHA256SUMS`. Check the **archive** before extracting
anything, from the directory that holds both files, and stop on a mismatch (without `SHA256SUMS`, ask the owner for
it; never extract an unchecked archive). `SHA256SUMS` comes from the same release, so this shows the archive arrived
intact, not who made it; the owner checks the release's attestation before attaching the files. Then take only the
`botport` binary out of it, in a fresh private temporary folder, and copy it to its own folder,
`/workspace/.botport/bin/botport`. Never copy over something that is already there (a file or a folder of that
name): stop and tell the owner instead. Run it as one block: it stops at the first failing step (`set -e`), so a
checksum mismatch never reaches the extract or the copy.
```sh
umask 077   # for this step and the ones after it
(
set -e
t="$(uname -m)-unknown-linux-musl"   # x86_64 or aarch64
# exactly the archive's own line (name compared whole), then the check, writing no file: must print "<archive>: OK"
line=$(awk -v f="botport-$t.tar.gz" '$2 == f' SHA256SUMS)
[ -n "$line" ] && [ "$(printf '%s\n' "$line" | wc -l)" -eq 1 ] || { echo "SHA256SUMS has no single line for botport-$t.tar.gz: stopping"; exit 1; }
printf '%s\n' "$line" | sha256sum -c -   # works with GNU and busybox sha256sum
mkdir -p /workspace/.botport/bin
chmod 700 /workspace/.botport /workspace/.botport/bin
[ ! -e /workspace/.botport/bin/botport ] || { echo "/workspace/.botport/bin/botport already exists: stopping"; exit 1; }
x=$(mktemp -d)
tar -xzf "botport-$t.tar.gz" -C "$x" "botport-$t/botport"
# only a regular file is taken (never a link the archive might hold); only that file is copied
[ -f "$x/botport-$t/botport" ] && [ ! -L "$x/botport-$t/botport" ] || { echo "the archive's botport is not a regular file: stopping"; exit 1; }
cp "$x/botport-$t/botport" /workspace/.botport/bin/botport
chmod 700 /workspace/.botport/bin/botport
/workspace/.botport/bin/botport --help | head -3
)
```

## Step 2 — find your agent id
```sh
ls /home/box/agent-data/agents/
```
Each 36-character folder with a `profile.json` is one agent. Open each `profile.json` and match its `name` to you.
Call your id `AID` and a short lowercase name `NAME` (e.g. `demo`) below: `NAME` is only letters `a-z`, digits and
`-` (never a `/`, `..`, a space or a control character), since it becomes part of file paths; quote paths that hold it.

## Step 3 — write your memory (only you can do this)
The tool cannot read your memory store. Write every durable memory you hold, one JSON object per line, to
`/workspace/exports/memory-drop/agents/AID/memory-agent.jsonl`:
```json
{"date":"2000-01-03","tier":"note","scope":"owner","source":"chat 2000-01-02","fact":"Example fact: the owner prefers short replies."}
```
- `tier`: `profile` (who/what you are, the owner), `log` (things that happened), or `note` (everything else).
- One fact per line, plain text, no secrets. Today's date in `date` if you don't know when you learned it.
- If you truly hold no memories, instead write `{"status":"none","date":"<today>"}` to `memory-status.json` in that
  folder.
- **Setup-only export** (the owner asked for one for the mirror, see [AGENT-MIRROR.md](AGENT-MIRROR.md)): write no
  memories at all; write `{"status":"setup-only"}` to `memory-status.json` in that folder instead.

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
- **New issue triage**: on an event — prompt: "Triage the new GitHub issue"
## Connectors
- **knowledge**: MCP server at https://demo.example.test/mcp (tools: find_items, health)
- **Web search**: built-in
## Credentials
- `DEMO_TOKEN`
```
A schedule started by an event (mail, a webhook, a GitHub issue) says "on an event" instead of a time.
Credential lines are the name only, nothing else (no location, no value).
**This file is copied as text** (into the bundle, then into Hermes cron prompts and checklists), so never put a secret
value anywhere in it: no passwords, tokens, keys or cookies, and no URLs with `user:pass@`, `?token=` or `#access_token=`.
botport routes or refuses values that look like secrets instead of writing them, but that is a detector, not a guarantee.

## Step 5 — export and verify
Each step runs only if the one before it succeeded (`&&`), so a failed export or verify never makes an archive:
```sh
/workspace/.botport/bin/botport export --agent-id AID \
  --describe /workspace/exports/describe.md \
  --out '/workspace/exports/NAME-g2h-bundle' --strict \
&& /workspace/.botport/bin/botport verify --bundle '/workspace/exports/NAME-g2h-bundle' \
&& (umask 077; tar -czf '/workspace/exports/NAME-g2h-bundle.tgz' -C /workspace/exports 'NAME-g2h-bundle')
```
(`--agent-data` defaults to `/home/box/agent-data`, the memory drop to `/workspace/exports/memory-drop`.) Chat
history is **not** exported unless the owner asked for it: then add `--with-history` (it reads
`/home/box/agent-data/search-index.db`; Hermes doesn't need it). If `--strict` stops on a warning — stale or missing
memory drop, missing describe file — fix it and re-run. Warnings about redacted history messages are fine.
The bundle holds the owner's personal data (memories, your description, and history if asked for); its files are
written `0600`. Hand it only to the owner, as below.

## Step 6 — hand it over
Send the owner `/workspace/exports/NAME-g2h-bundle.tgz` and paste the `export` and `verify` summaries (≤ 10 lines
each) plus one line: "Bundle for NAME (agent AID) is ready." Nothing else.

## Step 7 — other bots
Bots in the same box share `/home/box/agent-data`, so you can run Steps 1, 2 and 5 for each bot the owner names, with
`--agent-id '<its AID>'` and without `--strict`, output `'/workspace/exports/<its NAME>-g2h-bundle'`. Never message, ask or instruct another bot:
its bundle then holds what the box files hold (its name and the box's skills), and the export warns that its memory
drop and description are missing. Tell the owner in one line per bot what its bundle lacks; if they want a bot's
memory and description too, they send this file in that bot's own chat themselves. Hand over all bundles (Step 6),
one summary block per bot, plus a final line listing any bot that was skipped and why.

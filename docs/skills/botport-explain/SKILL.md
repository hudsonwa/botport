---
name: botport-explain
description: "Explain what botport is and does (export, import, mirror, the read-only Grok engine) and how to check the installed version. Read-only."
version: 1.0.0
author: botport
license: MIT
origin: plugin
metadata:
  hermes:
    tags: [botport, twin, explain]
    category: twin-pack
    origin: plugin
---

# botport: what it is and what it does

A read-only skill shipped with each botport release. It explains; it never runs an export, import, sync or
setup command.

## When to Use

The user asks "what is botport?", "what does it do to my bots?", "what does it touch?" or "which botport do I have?".

## What botport is

botport is a small open-source command-line program (source: github.com/hudsonwa/botport) that carries a bot
between platforms, such as a Grok bot and its Hermes twin, through a neutral bundle on disk.

- **export**: reads a bot's setup (instructions, skills, memory, routines) from one platform and writes a bundle.
  Reading changes nothing on that platform.
- **import**: writes a bundle into the other platform, item by item, and records what it wrote so it can be
  undone. It refuses to overwrite an item someone else changed until the owner chooses.
- **mirror**: keeps a pair in step on a schedule: the same import, run again, plus a short signed summary of
  recent activity for the bot on the other side.
- **Grok engine** (`botport grok`): reads the Grok desktop app's local files to list bots and read their setup.
  It is read-only: it never writes to the Grok app, never opens its secrets or cookies, never changes a Grok
  setting, and never clicks an approval.

## What it never touches

Passwords, keys, cookies and the Grok app's secrets file; the Hermes default profile; anything outside the
bot folders it was pointed at.

## Releases and checks

- Releases: the Releases page of the repository. Each release carries the archives, a `.sha256` for each,
  `SHA256SUMS`, and this file (`botport-explain-SKILL.md`), with its SHA-256 in the release notes.
- Build attestations: `gh attestation verify FILE --repo hudsonwa/botport` checks a download against GitHub's
  signed build record. The Grok Bot Twin plugin does this with `hermes twin verify`.
- Installed version: `botport --version` (or `botport version --json`).

## Rules

- Read only. Run at most `botport --version`; never run export, import, mirror, sync or setup for the user.
- Never guess a version: quote what the command printed.
- Do not print bot instructions, memory or skill text in the answer.

## Example (invented)

> botport 9.9.9 is installed. It read your "Acme Support" bot from the Grok app (read-only) and wrote its twin
> into Hermes. It never changes anything in Grok.

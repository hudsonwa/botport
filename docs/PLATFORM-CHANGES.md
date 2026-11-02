# Platform changes

A log of Grok Bot and Hermes behaviour that broke or surprised botport, and how the export brief or the tool
adapted. The Grok side is a closed platform with no export API, so the briefs depend on behaviour that can change
without notice. Newest first. If you hit a change, add an entry (date, what changed, what we did) in your PR.

## 2 Nov 2026

### Grok Bot: skills come from one shared library
A Grok Bot's skills live in a library shared by all the owner's Bots, so every export carries every skill, not just
the ones that Bot uses. botport added `skills_include = [...]` to the port spec so each Hermes profile takes only the
skills you list (`[]` takes none). See [USER-GUIDE.md](USER-GUIDE.md).

### Grok Bot: skill and instruction text exported with escaped newlines
The bot's self-export (`grok-bot-export` v1) can store skill texts and instructions with literal `\n` and no real
newlines. botport now unescapes text that has literal `\n` and no real newline, the same rule as upstream Hermes'
importer, so the Hermes skill files read as written. See [MODEL.md](MODEL.md).

### Grok Bot: replies to a routine run need a new wake
The Grok side reads g2h messages from a routine run, and that run can end while a reply is still on its way. A
reply or a queued op then waited until something woke the bot again. g2h now schedules a wake when the bot's active
window lapses, on `bye` and after `ask`, and the routine prompt now loops `g2h wait` for up to 12 rounds (it
stopped after 3 before). See
[ROUTINE-PROMPT.md](ROUTINE-PROMPT.md).

### Grok Bot: templates can't be imported by link
A Grok Bot cannot create another bot from an `x.ai` bot template link, so a "port this template" request sent from
one bot to another fails on the Grok side. Templates are ported by hand instead: the owner makes a Bot from the template,
that Bot exports itself, then `plan`/`apply` run as usual. The worked example shows the steps:
[examples/daily-wisdom.md](examples/daily-wisdom.md).

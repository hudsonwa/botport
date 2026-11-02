# Worked example: porting the Daily Wisdom template to Hermes

This example ports **Daily Wisdom**, a public Grok Bot template, into a Hermes profile and then tests the copy
with a parity battery.

## Credit
Daily Wisdom is by **Michael Gannotti / SMF Works**. All credit for the template goes to him.
- Templates: <https://github.com/smfworks/grok-bot-templates>
- SMF Works on GitHub: <https://github.com/smfworks>, website: <https://smfworks.com/>
- Michael on X: <https://x.com/michaelgannotti>

The template is shared through xAI's Grok Bot sharing system, under its terms, so **this repo contains no template
text**. You supply your own export when you run it. The repo keeps only the port spec, the battery, the commands and
the results.

| Template | Spec | Status |
|---|---|---|
| Daily Wisdom | [`examples/templates/daily-wisdom/botport.toml`](../../examples/templates/daily-wisdom/botport.toml) | ported, battery 4/5 |
| Roast My Feed | [`examples/templates/roast-my-feed/botport.toml`](../../examples/templates/roast-my-feed/botport.toml) | spec only, not ported yet |

## What the template has
Daily Wisdom gives you a short morning reading and one question to carry through the day, tracks a streak, and
writes a weekly reflection. It needs no connectors. Its export (grok-bot-export v1) has instructions, 7 memories,
2 user facts and 2 scheduled routines. It has no skills and no apps.

## 1. Port
Add the template to your Grok, ask the bot for its grok-bot-export v1, and save the file outside the repo. Then:

```sh
SPEC=examples/templates/daily-wisdom/botport.toml
botport plan  --bundle /path/to/daily-wisdom-grok-bot-export.json --spec $SPEC   # dry run
botport apply --bundle /path/to/daily-wisdom-grok-bot-export.json --spec $SPEC
```

`apply` creates:
- the memory skill `daily-wisdom-memory`, which holds the 7 memories;
- `SOUL.md`, with the export's instructions in an owned `persona` section;
- 3 config keys and `USER.md`;
- 2 cron jobs, both **paused**, so nothing runs until you say so.

## 2. SOUL: check the persona
Open `SOUL.md` in the new profile. The template's instructions sit between the
`<!-- botport:begin persona -->` and `<!-- botport:end persona -->` markers. Anything you write outside
those markers is yours, and `sync` leaves it alone.

An early run of this example had no persona section, so the instructions had to be pasted in by hand. botport now
writes them itself; that change is the "instructions fix" in the results below.

## 3. Battery
Before you run `test`:
- **Pick a model.** The spec uses a placeholder (`example-model`). Point the profile's `model:` at a model you can
  reach.
- **Give the profile a gateway.** If other profiles run their own gateways, Hermes refuses a new one
  (`profile_parked`). For a throwaway profile, set `gateway.standalone: true` and run
  `hermes -p tpl-daily-wisdom gateway run`.

```sh
botport test --profile tpl-daily-wisdom --battery examples/templates/daily-wisdom/battery.json
```

The [battery](../../examples/templates/daily-wisdom/battery.json) has 5 cases: one persona case, three memory facts,
and the morning routine asked for on demand.

## Results
The model was a local OpenAI-compatible model.

| Run | Setup | Result |
|---|---|---|
| 1 | as applied, no gateway | 0/5: every turn refused (`profile_parked`) |
| 2 | standalone gateway, first battery draft | 0/5: no persona yet, and two of the battery's own checks were wrong |
| 3 | instructions fix, battery corrected (the committed battery) | **4/5** |

In run 3 the persona, streak, quote and on-demand reading cases pass. The tradition-rotation case fails: the model
answered from general knowledge and **didn't load the memory skill**, so it missed the rule stored there. This is
an honest limit of the port. Memories become a skill, and a model only follows them if it decides to open that
skill.

## What carries over
| Template feature | Hermes equivalent | Status |
|---|---|---|
| Instructions | `persona` section of `SOUL.md` | works |
| Memories | `daily-wisdom-memory` skill | works if the model loads the skill |
| User facts | echo-prevention facts | partial: not written to `USER.md` |
| Daily routine | paused cron job, same schedule | works; also works on demand |
| Weekly reflection page | paused cron job | partial: Hermes has no Grok Pages, so it is plain text |
| Files in the Grok workspace | none by default | manual: give the profile a file tool and a folder |
| Image card | none | not portable without an image tool |

## Try the spec without a real export
[`examples/templates/check.sh`](../../examples/templates/check.sh) runs `plan` on each template spec against a tiny
made-up fixture. It checks that the specs parse; it doesn't port anything real.

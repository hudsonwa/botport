//! P3 routines and usage.
//!
//! What app 1.1.0 exposes (control/GROUND-TRUTH.md): the routine list and enable/disable are direct calls
//! (`listTeamBotAutomations`, `setTeamBotAutomationEnabled`); there is no call to create or run one, so `add` and
//! `run` ask the bot in chat; `remove` is the routine row's menu → Delete → confirm in the app (debug-port UI), and
//! where the transport cannot do that, the bot is asked (after the routine is disabled by the direct call).
//! Every PASS is checked against a fresh routine list (`routines-list`), not the call that did the work.
//!
//! Usage: `getCursorWeeklyUsage` + `getCursorUsageSummary`; [`Engine::usage_gate`] runs before every paid turn
//! (send, ask, routine add/run): `warn` presets add a warning at the threshold, `refuse` exits 4.
use crate::engine::{halt, terr, Engine, ExitCode, Outcome, Proof, WaitOpts, R};
use crate::model::*;
use crate::policy::{Action, UsageCheck};
use crate::transport::{Cap, TransportError};

/// What `routines add` asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutineSpec {
    pub name: String,
    /// Five-field cron (`m h dom mon dow`).
    pub schedule: String,
    pub tz: Option<String>,
    pub prompt: String,
    pub disabled: bool,
}

/// The chat message that asks a bot to create a routine (1.1.0 has no create call).
pub fn add_prompt(s: &RoutineSpec) -> String {
    format!(
        "Please create a routine for yourself named \"{}\" on the cron schedule \"{}\" in time zone {}. \
         Its prompt: <<<{}>>>. Do not run it now. When it is saved, reply ROUTINE OK.",
        s.name,
        s.schedule,
        s.tz.as_deref().unwrap_or("UTC"),
        s.prompt
    )
}

/// Inverse of [`add_prompt`] (the fake transport uses it to play the bot).
pub fn parse_add_prompt(t: &str) -> Option<RoutineSpec> {
    let name = between(t, "named \"", "\" on the cron schedule")?;
    let schedule = between(t, "on the cron schedule \"", "\" in time zone ")?;
    let tz = between(t, "\" in time zone ", ". Its prompt:")?;
    let prompt = between(t, "Its prompt: <<<", ">>>")?;
    Some(RoutineSpec {
        name,
        schedule,
        tz: Some(tz),
        prompt,
        disabled: false,
    })
}

pub fn run_prompt(r: &Routine) -> String {
    format!(
        "Please run your routine \"{}\" (id {}) now, once, as a manual run of that routine. Reply RUN OK when it has started.",
        r.name, r.slug
    )
}

pub fn parse_run_prompt(t: &str) -> Option<String> {
    t.starts_with("Please run your routine \"")
        .then(|| between(t, "routine \"", "\" (id "))
        .flatten()
}

pub fn remove_prompt(r: &Routine) -> String {
    format!(
        "Please delete your routine \"{}\" (id {}) permanently. Do not delete any other routine. Reply ROUTINE DELETED when it is gone.",
        r.name, r.slug
    )
}

pub fn parse_remove_prompt(t: &str) -> Option<String> {
    t.starts_with("Please delete your routine \"")
        .then(|| between(t, "routine \"", "\" (id "))
        .flatten()
}

fn between(t: &str, a: &str, b: &str) -> Option<String> {
    let i = t.find(a)? + a.len();
    let j = t[i..].find(b)? + i;
    Some(t[i..j].to_string())
}

fn norm_cron(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A routine by automation id, slug, exact name; with `exact` false also a case-insensitive name or unique prefix.
pub fn pick<'a>(list: &'a [Routine], sel: &str, exact: bool) -> Result<&'a Routine, String> {
    if let Some(r) = list
        .iter()
        .find(|r| r.id == sel || r.slug == sel || r.name == sel)
    {
        return Ok(r);
    }
    if exact {
        return Err(format!(
            "no routine named exactly {sel:?} (its id, slug or exact name)"
        ));
    }
    let l = sel.to_lowercase();
    let eq: Vec<&Routine> = list.iter().filter(|r| r.name.to_lowercase() == l).collect();
    let m = if eq.is_empty() {
        list.iter()
            .filter(|r| r.name.to_lowercase().starts_with(&l))
            .collect()
    } else {
        eq
    };
    match m.len() {
        1 => Ok(m[0]),
        0 => Err(format!("no routine matches {sel:?}")),
        _ => Err(format!(
            "{sel:?} is ambiguous: {}",
            m.iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

impl Engine {
    fn list_routines(&mut self, what: &str, id: &str) -> R<Vec<Routine>> {
        let id = id.to_string();
        self.with_t(Cap::RoutinesList, |t| t.routines(&id))
            .map_err(|e| terr(what, e))
    }

    /// `routines <bot>` (list).
    pub fn routines(&mut self, sel: &str) -> Outcome<Vec<Routine>> {
        let w = self.read_warnings();
        let r = (|| {
            let what = "routines";
            let bot = self.find(what, sel)?;
            self.decide(what, Action::Read, Some(&bot))?;
            let list = self.list_routines(what, &bot.id)?;
            let on = list.iter().filter(|r| r.enabled).count();
            let p = Proof::pass(
                format!("{} routines, {on} enabled", list.len()),
                "self-check",
                "self-check",
            );
            Ok((bot.id, Outcome::ok(list, p)))
        })();
        self.done("routines list", r, w)
    }

    fn done<T>(&mut self, cmd: &str, r: R<(String, Outcome<T>)>, w: Vec<String>) -> Outcome<T> {
        let (id, o) = match r {
            Ok((id, o)) => (Some(id), o),
            Err(h) => (None, Outcome::from_halt(h)),
        };
        let mut o = self.finish(cmd, id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    /// `routines <bot> enable|disable <routine>`: the direct call; PASS when a fresh list agrees.
    pub fn routine_toggle(&mut self, sel: &str, rsel: &str, on: bool) -> Outcome<RoutineChange> {
        let mut w = vec![];
        let action = if on { "enable" } else { "disable" };
        let r = (|| {
            let what = &format!("routines {action}");
            let bot = self.find(what, sel)?;
            self.decide(what, Action::RoutineChange, Some(&bot))?;
            let list = self.list_routines(what, &bot.id)?;
            let r = pick(&list, rsel, false)
                .map_err(|e| {
                    halt(
                        ExitCode::Other,
                        what,
                        e,
                        Some("botport grok routines <bot>"),
                    )
                })?
                .clone();
            if !r.can.toggle {
                return Err(halt(
                    ExitCode::Refused,
                    what,
                    format!("{}: the app does not let this account toggle it", r.name),
                    None,
                ));
            }
            self.version_gate(what, &mut w)?;
            let (id, rid) = (bot.id.clone(), r.id.clone());
            if r.enabled != on {
                self.with_t(Cap::RoutinesToggle, |t| {
                    t.routine_set_enabled(&id, &rid, on)
                })
                .map_err(|e| terr(what, e))?;
            }
            let after = self.list_routines(what, &id)?;
            let now = after.iter().find(|x| x.id == rid).cloned().ok_or_else(|| {
                halt(
                    ExitCode::Other,
                    what,
                    format!("{} is gone from the list", r.name),
                    None,
                )
            })?;
            if now.enabled != on {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    format!("the list still shows enabled={}", now.enabled),
                    None,
                ));
            }
            let p = Proof::pass(
                format!(
                    "{} {}{}",
                    r.name,
                    if on { "enabled" } else { "disabled" },
                    if r.enabled == on { " (already)" } else { "" }
                ),
                format!("list shows enabled={on}"),
                "routines-list",
            );
            Ok((
                id,
                Outcome::ok(
                    RoutineChange {
                        bot,
                        action: action.into(),
                        routine: Some(now),
                        via: "ipc".into(),
                        messages: vec![],
                    },
                    p,
                ),
            ))
        })();
        self.done(&format!("routines {action}"), r, w)
    }

    /// Ask the bot in chat (send + settle loop). Exit 2/3 from the wait end the command with that exit.
    fn ask_bot(
        &mut self,
        what: &str,
        bot: &Bot,
        msg: &str,
        opts: &WaitOpts,
        w: &mut Vec<String>,
    ) -> R<Vec<Message>> {
        let (b, key, _) = self.send_inner(&bot.id, msg, &[], w)?;
        let o = self.wait_loop(b, Some(key), opts);
        match o.exit {
            ExitCode::Done => Ok(o.value.map(|v| v.messages).unwrap_or_default()),
            e => Err(halt(e, what, o.proof.check, o.hint.as_deref())),
        }
    }

    /// Poll the list (every 2 s, up to `secs`) until `ok` holds.
    fn poll_list(
        &mut self,
        what: &str,
        id: &str,
        secs: u64,
        mut ok: impl FnMut(&[Routine]) -> bool,
    ) -> R<(bool, Vec<Routine>)> {
        let t0 = self.now();
        loop {
            let l = self.list_routines(what, id)?;
            if ok(&l) {
                return Ok((true, l));
            }
            if self.now() - t0 >= secs * 1000 {
                return Ok((false, l));
            }
            self.clock.sleep_ms(2000);
        }
    }

    /// `routines <bot> add --name N --schedule CRON [--tz TZ] --prompt P [--disabled]`: asks the bot (1.1.0 has
    /// no create call). PASS: a fresh list shows the name with the asked schedule.
    pub fn routine_add(
        &mut self,
        sel: &str,
        spec: &RoutineSpec,
        opts: &WaitOpts,
    ) -> Outcome<RoutineChange> {
        let mut w = vec![];
        let r = (|| {
            let what = "routines add";
            if spec.name.trim().is_empty() || spec.schedule.split_whitespace().count() != 5 {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    "needs --name and a five-field cron --schedule (\"0 9 * * 1\")",
                    None,
                ));
            }
            if spec.prompt.trim().is_empty() {
                return Err(halt(ExitCode::Other, what, "needs --prompt", None));
            }
            let bot = self.find(what, sel)?;
            self.decide(what, Action::RoutineChange, Some(&bot))?;
            let before = self.list_routines(what, &bot.id)?;
            if pick(&before, &spec.name, true).is_ok() {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    format!("{} already exists", spec.name),
                    Some("pick another --name"),
                ));
            }
            let msgs = self.ask_bot(what, &bot, &add_prompt(spec), opts, &mut w)?;
            let want = spec.name.to_lowercase();
            let (seen, list) = self.poll_list(what, &bot.id, 30, |l| {
                l.iter().any(|r| r.name.to_lowercase() == want)
            })?;
            let mut r = list
                .into_iter()
                .find(|r| r.name.to_lowercase() == want)
                .filter(|_| seen)
                .ok_or_else(|| {
                    halt(
                        ExitCode::Other,
                        what,
                        format!(
                            "the bot replied but {} is not in the routine list",
                            spec.name
                        ),
                        Some("botport grok read <bot>"),
                    )
                })?;
            if norm_cron(&r.schedule) != norm_cron(&spec.schedule) {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    format!(
                        "{} is listed with schedule {:?}, asked for {:?}",
                        r.name, r.schedule, spec.schedule
                    ),
                    Some("botport grok routines <bot> remove <name>, then add again"),
                ));
            }
            if let (Some(a), Some(b)) = (&spec.tz, &r.tz) {
                if a != b {
                    w.push(format!(
                        "{} is listed in time zone {b}, asked for {a}",
                        r.name
                    ));
                }
            }
            if spec.disabled && r.enabled {
                let (id, rid) = (bot.id.clone(), r.id.clone());
                self.with_t(Cap::RoutinesToggle, |t| {
                    t.routine_set_enabled(&id, &rid, false)
                })
                .map_err(|e| terr(what, e))?;
                let l = self.list_routines(what, &id)?;
                r = l.into_iter().find(|x| x.id == rid).ok_or_else(|| {
                    halt(
                        ExitCode::Other,
                        what,
                        "the routine is gone after disabling it",
                        None,
                    )
                })?;
                if r.enabled {
                    return Err(halt(
                        ExitCode::Other,
                        what,
                        "the list still shows it enabled",
                        None,
                    ));
                }
            }
            let p = Proof::pass(
                format!("{} listed with schedule {}", r.name, r.schedule),
                format!(
                    "routine list{}",
                    if spec.disabled { "; enabled=false" } else { "" }
                ),
                "routines-list",
            );
            Ok((
                bot.id.clone(),
                Outcome::ok(
                    RoutineChange {
                        bot,
                        action: "add".into(),
                        routine: Some(r),
                        via: "bot".into(),
                        messages: msgs,
                    },
                    p,
                ),
            ))
        })();
        self.done("routines add", r, w)
    }

    /// `routines <bot> remove <routine>`: exact id, slug or name only (floor 4). The app's menu → Delete → confirm
    /// where the transport can; otherwise the routine is disabled by the direct call, then the bot is asked.
    pub fn routine_remove(
        &mut self,
        sel: &str,
        exact: &str,
        confirm: Option<&str>,
        yes: bool,
        opts: &WaitOpts,
    ) -> Outcome<RoutineChange> {
        let mut w = vec![];
        let r = (|| {
            let what = "routines remove";
            let bot = self.find(what, sel)?;
            self.decide(what, Action::RoutineChange, Some(&bot))?;
            let list = self.list_routines(what, &bot.id)?;
            let r = pick(&list, exact, true)
                .map_err(|e| {
                    halt(
                        ExitCode::Refused,
                        what,
                        format!("refused by floor 4: {e}"),
                        Some("botport grok routines <bot>"),
                    )
                })?
                .clone();
            // the slug is an exact name too
            let typed_ok = if exact == r.slug {
                r.name.as_str()
            } else {
                exact
            };
            let confirm = confirm.map(|c| if c == r.slug { r.name.as_str() } else { c });
            self.decide_destructive(what, typed_ok, confirm, yes, &r.id, &r.name)?;
            self.version_gate(what, &mut w)?;
            let (id, rid) = (bot.id.clone(), r.id.clone());
            let mut via = "ui";
            let mut msgs = vec![];
            match self.with_t(Cap::RoutinesRemove, |t| t.routine_remove(&id, &rid)) {
                Ok(()) => {}
                Err(TransportError::Unsupported(_)) => {
                    via = "bot";
                    if r.enabled && r.can.toggle {
                        let _ = self.with_t(Cap::RoutinesToggle, |t| {
                            t.routine_set_enabled(&id, &rid, false)
                        });
                    }
                    msgs = self.ask_bot(what, &bot, &remove_prompt(&r), opts, &mut w)?;
                }
                Err(e) => return Err(terr(what, e)),
            }
            let (gone, _) = self.poll_list(what, &id, 30, |l| !l.iter().any(|x| x.id == rid))?;
            if !gone {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    format!("{} is still in the routine list", r.name),
                    None,
                ));
            }
            let p = Proof::pass(
                format!("{} removed", r.name),
                "absent from the routine list",
                "routines-list",
            );
            Ok((
                id,
                Outcome::ok(
                    RoutineChange {
                        bot,
                        action: "remove".into(),
                        routine: None,
                        via: via.into(),
                        messages: msgs,
                    },
                    p,
                ),
            ))
        })();
        self.done("routines remove", r, w)
    }

    /// `routines <bot> run <routine>`: asks the bot to run it now (1.1.0 has no Run control). PASS needs a new run
    /// in the routine list (more runs, a newer last run, or one running).
    pub fn routine_run(
        &mut self,
        sel: &str,
        rsel: &str,
        opts: &WaitOpts,
    ) -> Outcome<RoutineChange> {
        let mut w = vec![];
        let r = (|| {
            let what = "routines run";
            let bot = self.find(what, sel)?;
            self.decide(what, Action::RoutineChange, Some(&bot))?;
            let list = self.list_routines(what, &bot.id)?;
            let r = pick(&list, rsel, false)
                .map_err(|e| {
                    halt(
                        ExitCode::Other,
                        what,
                        e,
                        Some("botport grok routines <bot>"),
                    )
                })?
                .clone();
            let msgs = self.ask_bot(what, &bot, &run_prompt(&r), opts, &mut w)?;
            let rid = r.id.clone();
            let (seen, l) = self.poll_list(what, &bot.id, 60, |l| {
                l.iter().any(|x| {
                    x.id == rid && (x.runs > r.runs || x.running || x.last_run > r.last_run)
                })
            })?;
            let now = l.into_iter().find(|x| x.id == rid);
            if !seen {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    "the bot replied, but no new run is in the routine list after 60 s",
                    Some(
                        "botport grok read <bot> (the bot may have done the work in chat instead)",
                    ),
                ));
            }
            let p = Proof::pass(
                format!("{} run started", r.name),
                "a new run is in the routine list",
                "routines-list",
            );
            Ok((
                bot.id.clone(),
                Outcome::ok(
                    RoutineChange {
                        bot,
                        action: "run".into(),
                        routine: now,
                        via: "bot".into(),
                        messages: msgs,
                    },
                    p,
                ),
            ))
        })();
        self.done("routines run", r, w)
    }

    /// `usage`.
    pub fn usage(&mut self) -> Outcome<Usage> {
        let w = self.read_warnings();
        let o = match self.with_t(Cap::Usage, |t| t.usage()) {
            Ok(u) => {
                self.usage_seen = Some(Some(u.clone()));
                let reset = u
                    .resets_at
                    .as_deref()
                    .map(|t| format!("; resets {}", crate::util::local_from_rfc3339(t)))
                    .unwrap_or_default();
                let p = Proof::pass(
                    format!("{}% used{reset}", u.percent_used),
                    "self-check",
                    "self-check",
                );
                let mut o = Outcome::ok(u.clone(), p);
                o.warnings = self.usage_warning(&u).into_iter().collect();
                o
            }
            Err(e) => Outcome::from_halt(terr("usage", e)),
        };
        let mut o = self.finish("usage", None, o);
        o.warnings.extend(w);
        o
    }

    fn usage_warning(&self, u: &Usage) -> Option<String> {
        let pct = match self.policy.usage_check {
            UsageCheck::Warn(p) | UsageCheck::Refuse(p) => p,
        };
        if !u.available {
            Some("Grok reports no usage left on this plan".into())
        } else if u.percent_used >= pct {
            Some(format!(
                "{}% of the weekly Grok usage is used (usage_check at {pct}%)",
                u.percent_used
            ))
        } else {
            None
        }
    }

    /// The usage check before a paid turn (send, ask, routine add/run). Read once per engine. warn: a warning at
    /// the threshold; refuse: exit 4 naming `usage_check`. A usage read that fails adds a warning; a transport
    /// without usage skips the check.
    pub(crate) fn usage_gate(&mut self, what: &str, warnings: &mut Vec<String>) -> R<()> {
        let u = match &self.usage_seen {
            Some(u) => u.clone(),
            None => {
                let u = match self.with_t(Cap::Usage, |t| t.usage()) {
                    Ok(u) => Some(u),
                    Err(TransportError::Unsupported(_)) => None,
                    Err(e) => {
                        warnings.push(format!(
                            "usage check skipped: usage could not be read ({e})"
                        ));
                        None
                    }
                };
                self.usage_seen = Some(u.clone());
                u
            }
        };
        let Some(u) = u else { return Ok(()) };
        let Some(msg) = self.usage_warning(&u) else {
            return Ok(());
        };
        match self.policy.usage_check {
            UsageCheck::Refuse(_) => Err(halt(
                ExitCode::Refused,
                what,
                format!("refused by usage_check: {msg}"),
                Some("botport grok usage; wait for the reset, or botport grok policy preset balanced (warns instead)"),
            )),
            UsageCheck::Warn(_) => {
                warnings.push(msg);
                Ok(())
            }
        }
    }
}

impl Engine {
    /// Paid turns botport recorded in the last 7 days.
    pub fn turns_this_week(&self) -> u64 {
        self.turns.this_week(self.now())
    }

    /// The weekly turn budget before a paid call: a warning from `usage_warn_pct` of `usage_budget_week`; at the
    /// budget, exit 4 naming `usage_hard_cap` (or a warning when it is off). No budget: no check.
    pub(crate) fn budget_gate(&mut self, what: &str, warnings: &mut Vec<String>) -> R<()> {
        let Some(budget) = self.policy.usage_budget_week else {
            return Ok(());
        };
        let used = self.turns_this_week();
        if used >= budget {
            let msg = format!("{used} turns by botport this week, budget {budget}");
            if self.policy.usage_hard_cap {
                return Err(halt(
                    ExitCode::Refused,
                    what,
                    format!("refused by usage_hard_cap: {msg}"),
                    Some("botport grok usage; raise usage_budget_week, or botport grok policy set usage_hard_cap off (warns instead)"),
                ));
            }
            warnings.push(format!("{msg} (usage_budget_week reached)"));
        } else if budget > 0
            && (used + 1) as f64 * 100.0 >= budget as f64 * self.policy.usage_warn_pct
        {
            warnings.push(format!(
                "turn {} of a weekly budget of {budget} (usage_warn_pct at {}%)",
                used + 1,
                self.policy.usage_warn_pct
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_round_trip() {
        let s = RoutineSpec {
            name: "nightly digest".into(),
            schedule: "0 9 * * 1".into(),
            tz: Some("UTC".into()),
            prompt: "Summarise the day.".into(),
            disabled: false,
        };
        assert_eq!(parse_add_prompt(&add_prompt(&s)).unwrap(), s);
    }
}

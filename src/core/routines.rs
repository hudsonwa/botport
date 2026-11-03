//! Routine parity (docs/design/MIRROR.md section 9): two lists of neutral [`Schedule`]s compared by name, one
//! status per routine, and the normalised neutral form a mirrored pair hashes (section 3). Platform-free: the
//! two sides are `a` and `b`; the caller names them.
//!
//! ```
//! use botport::core::routines::normalize_schedule;
//!
//! assert_eq!(normalize_schedule("every 1d at 05:15"), "15 5 * * *");
//! assert_eq!(normalize_schedule(" 0  9 * * MON-FRI "), "0 9 * * 1-5");
//! ```
use super::model::{Schedule, ScheduleKind};
use super::util::sha256_hex;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// What a comparison needs besides the two lists.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// The owner's timezone: what an empty timezone means on either side.
    pub default_tz: String,
    /// Names of routines that botport itself paused on side a awaiting approval (an expected `paused-differs`).
    pub paused_by_policy_a: BTreeSet<String>,
    /// The same for side b.
    pub paused_by_policy_b: BTreeSet<String>,
}

/// One routine's parity, worst first: a routine gets the first status that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Schedule, timezone, prompt and enabled state match.
    Same,
    /// The normalised schedules differ (or one side is event-triggered and the other is not).
    ScheduleDiffers,
    /// Same schedule, different timezone.
    TzDiffers,
    /// The prompts differ (shown as short digests, never as text).
    PromptDiffers,
    /// One side is paused.
    PausedDiffers,
    /// Present on side a only.
    OnlyA,
    /// Present on side b only.
    OnlyB,
    /// Event-triggered, on one side only: nothing on the other side can hold it. Not a failure.
    EventOnly,
    /// The name is listed more than once on one side (after any prefix the caller removes): no entry is compared,
    /// each needs a look.
    DuplicateName,
}

impl Status {
    /// The status as printed (`only-a`/`only-b` are renamed to the platforms by the caller).
    pub fn word(self) -> &'static str {
        match self {
            Status::Same => "same",
            Status::ScheduleDiffers => "schedule-differs",
            Status::TzDiffers => "tz-differs",
            Status::PromptDiffers => "prompt-differs",
            Status::PausedDiffers => "paused-differs",
            Status::OnlyA => "only-a",
            Status::OnlyB => "only-b",
            Status::EventOnly => "event-only",
            Status::DuplicateName => "duplicate-name",
        }
    }
}

/// One line of the parity report: `a` and `b` are what each side has for the field that differs (a normalised
/// schedule, a timezone, a short prompt digest, `enabled`/`paused`, `event`, how many entries carry the name for
/// `duplicate-name`; empty when absent or `same`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Parity {
    /// Routine name.
    pub name: String,
    /// The first status that applies.
    pub status: Status,
    /// Side a's value for the differing field.
    pub a: String,
    /// Side b's value for the differing field.
    pub b: String,
    /// `paused-differs` only: the paused side is one botport paused awaiting approval.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub expected: bool,
}

impl Parity {
    /// Fine as it is: `same`, `event-only`, or a `paused-differs` that botport's own policy explains.
    pub fn ok(&self) -> bool {
        match self.status {
            Status::Same | Status::EventOnly => true,
            Status::PausedDiffers => self.expected,
            _ => false,
        }
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
/// Each cron field's range: minute, hour, day of month, month, day of week (7 is Sunday too).
const RANGES: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];

/// One cron value: a number (leading zeros dropped) or, in the month and weekday fields, a three-letter name.
fn atom(v: &str, field: usize) -> Option<u32> {
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse().ok();
    }
    let l = v.to_ascii_lowercase();
    match field {
        3 => MONTHS.iter().position(|x| *x == l).map(|i| i as u32 + 1),
        4 => DAYS.iter().position(|x| *x == l).map(|i| i as u32),
        _ => None,
    }
}

/// Sorted values as a cron list: runs of three or more as `a-b`.
fn runs(vals: &BTreeSet<u32>) -> String {
    let v: Vec<u32> = vals.iter().copied().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        if j - i >= 2 {
            out.push(format!("{}-{}", v[i], v[j]));
        } else {
            out.extend(v[i..=j].iter().map(u32::to_string));
        }
        i = j + 1;
    }
    out.join(",")
}

/// One cron field in canonical form, or None when it is not a plain five-field cron field. A field without steps
/// becomes its sorted value list (weekday 7 read as 0); a field with steps keeps its shape, values as numbers.
fn field(f: &str, i: usize) -> Option<String> {
    let (lo, hi) = RANGES[i];
    if f == "*" {
        return Some(f.into());
    }
    if !f.contains('/') {
        let mut vals = BTreeSet::new();
        for part in f.split(',') {
            let (a, b) = part.split_once('-').unwrap_or((part, part));
            let (a, b) = (atom(a, i)?, atom(b, i)?);
            if a > b || a < lo || b > hi {
                return None;
            }
            for v in a..=b {
                vals.insert(if i == 4 && v == 7 { 0 } else { v });
            }
        }
        return Some(runs(&vals));
    }
    let mut parts = vec![];
    for part in f.split(',') {
        let (base, step) = part.split_once('/').unwrap_or((part, ""));
        let mut s = match base {
            "*" => "*".to_string(),
            _ => {
                let (a, b) = base.split_once('-').unwrap_or((base, base));
                let (a, b) = (atom(a, i)?, atom(b, i)?);
                if a > b || a < lo || b > hi {
                    return None;
                }
                if base.contains('-') {
                    format!("{a}-{b}")
                } else {
                    a.to_string()
                }
            }
        };
        if part.contains('/') {
            let n = atom(step, 0).filter(|n| *n >= 1)?;
            s += &format!("/{n}");
        }
        parts.push(s);
    }
    Some(parts.join(","))
}

/// `every <n><unit> [at HH:MM]` -> the cron line that runs at the same times, when one does; the interval in
/// minutes otherwise (`every 90m`). None when the text is not an interval.
fn interval(s: &str) -> Option<String> {
    let l = s.to_ascii_lowercase();
    let rest = l.strip_prefix("every ")?;
    let (dur, at) = match rest.split_once(" at ") {
        Some((d, t)) => (d.trim(), Some(t.trim())),
        None => (rest.trim(), None),
    };
    let digits = dur.bytes().take_while(u8::is_ascii_digit).count();
    let n: u32 = if digits == 0 {
        1
    } else {
        dur[..digits].parse().ok()?
    };
    let mult = match dur[digits..].trim_start() {
        "m" | "min" | "mins" | "minute" | "minutes" if digits > 0 => 1,
        "minute" => 1,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60,
        "d" | "day" | "days" => 1440,
        _ => return None,
    };
    let m = n.checked_mul(mult).filter(|m| *m > 0)?;
    if let Some(t) = at {
        // only a daily interval with a clock time is a cron line; any other `at` stays as written
        let (h, mm) = t.split_once(':')?;
        let ok =
            |x: &str, w: usize| (1..=w).contains(&x.len()) && x.bytes().all(|b| b.is_ascii_digit());
        if m != 1440 || !ok(h, 2) || mm.len() != 2 || !ok(mm, 2) {
            return None;
        }
        let (h, mm): (u32, u32) = (h.parse().ok()?, mm.parse().ok()?);
        return (h <= 23 && mm <= 59).then(|| format!("{mm} {h} * * *"));
    }
    Some(match m {
        1 => "* * * * *".into(),
        m if m < 60 && 60 % m == 0 => format!("*/{m} * * * *"),
        60 => "0 * * * *".into(),
        m if m % 60 == 0 && m < 1440 && 1440 % m == 0 => format!("0 */{} * * *", m / 60),
        // a plain daily interval runs relative to when it started, not at midnight: compared as written in minutes
        m => format!("every {m}m"),
    })
}

/// A schedule in canonical form, so one schedule spelled two ways compares equal: a five-field cron with
/// whitespace collapsed, day and month names as numbers, value lists sorted, 7 read as Sunday (0); an interval a
/// cron line expresses exactly as that line (`every 6h` -> `0 */6 * * *`, `every 1d at 05:15` -> `15 5 * * *`),
/// any other interval in minutes (`every 90m`); anything else as written, whitespace collapsed.
pub fn normalize_schedule(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(c) = interval(&one) {
        return c;
    }
    let f: Vec<&str> = one.split(' ').collect();
    if f.len() == 5 {
        let canon: Option<Vec<String>> = f.iter().enumerate().map(|(i, x)| field(x, i)).collect();
        if let Some(c) = canon {
            return c.join(" ");
        }
    }
    one
}

/// Spellings of UTC (case-insensitive) that all compare as `UTC`.
const UTC: [&str; 9] = [
    "utc",
    "etc/utc",
    "etc/universal",
    "universal",
    "etc/zulu",
    "zulu",
    "gmt",
    "etc/gmt",
    "etc/gmt0",
];

/// A timezone as it may be printed: a name from the other side is shown only when it uses the tz database's
/// alphabet (letters, digits, `_`, `+`, `-`, `/`), so no control or direction character reaches a terminal or report.
pub fn shown_tz(tz: &str) -> String {
    if !tz.is_empty()
        && tz.len() <= 64
        && tz
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '/'))
    {
        tz.to_string()
    } else {
        "(a zone name with unexpected characters)".into()
    }
}

/// A timezone in canonical form: trimmed, empty -> `default_tz`, the spellings of UTC -> `UTC`.
pub fn normalize_tz(tz: &str, default_tz: &str) -> String {
    let t = match tz.trim() {
        "" => default_tz.trim(),
        t => t,
    };
    if UTC.contains(&t.to_ascii_lowercase().as_str()) {
        "UTC".into()
    } else {
        t.into()
    }
}

/// A prompt in canonical form for comparing and hashing: CRLF -> LF, trailing whitespace per line and at the end
/// dropped.
pub fn normalize_prompt(p: &str) -> String {
    p.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// The first 8 hex digits of the sha256 of the canonical prompt: what a report shows instead of the text.
pub fn prompt_digest(p: &str) -> String {
    sha256_hex(normalize_prompt(p).as_bytes())[..8].to_string()
}

/// The schedule as compared: `event` for an event trigger, else the normalised schedule (empty when none given).
fn when(s: &Schedule) -> String {
    match s.kind {
        ScheduleKind::Event => "event".into(),
        ScheduleKind::Scheduled => normalize_schedule(s.cron.as_deref().unwrap_or("")),
    }
}

/// The neutral form of a routine: `{schedule, tz, kind, prompt, enabled}`, normalised (an event has no schedule
/// and no timezone). `paused_by_policy` leaves out `enabled`, for a routine botport paused awaiting approval, so a
/// paused copy is in step with an enabled one.
pub fn neutral(s: &Schedule, default_tz: &str, paused_by_policy: bool) -> serde_json::Value {
    let event = s.kind == ScheduleKind::Event;
    let mut v = json!({
        "schedule": if event { None } else { Some(when(s)) },
        "tz": if event { String::new() } else { normalize_tz(&s.tz, default_tz) },
        "kind": if event { "event" } else { "scheduled" },
        "prompt": normalize_prompt(&s.action),
    });
    if !paused_by_policy {
        v["enabled"] = json!(s.enabled);
    }
    v
}

/// sha256 (hex) over the neutral form as compact sorted-key JSON: the digest a cross-side comparison uses.
pub fn neutral_digest(s: &Schedule, default_tz: &str, paused_by_policy: bool) -> String {
    let v = neutral(s, default_tz, paused_by_policy);
    sha256_hex(serde_json::to_string(&v).expect("json").as_bytes())
}

fn on_off(enabled: bool) -> String {
    if enabled { "enabled" } else { "paused" }.into()
}

/// One routine present on both sides.
fn pair(name: &str, x: &Schedule, y: &Schedule, o: &Opts) -> Parity {
    let p = |status, a: String, b: String| Parity {
        name: name.to_string(),
        status,
        a,
        b,
        expected: false,
    };
    let (wx, wy) = (when(x), when(y));
    if wx != wy {
        return p(Status::ScheduleDiffers, wx, wy);
    }
    let (tx, ty) = (
        normalize_tz(&x.tz, &o.default_tz),
        normalize_tz(&y.tz, &o.default_tz),
    );
    if x.kind == ScheduleKind::Scheduled && tx != ty {
        return p(Status::TzDiffers, shown_tz(&tx), shown_tz(&ty));
    }
    if normalize_prompt(&x.action) != normalize_prompt(&y.action) {
        return p(
            Status::PromptDiffers,
            prompt_digest(&x.action),
            prompt_digest(&y.action),
        );
    }
    if x.enabled != y.enabled {
        let expected = (!x.enabled && o.paused_by_policy_a.contains(name))
            || (!y.enabled && o.paused_by_policy_b.contains(name));
        return Parity {
            expected,
            ..p(Status::PausedDiffers, on_off(x.enabled), on_off(y.enabled))
        };
    }
    p(Status::Same, String::new(), String::new())
}

/// Compare two sides' routines by exact name: one [`Parity`] per name, sorted by name. A name listed more than once
/// on either side is `duplicate-name` (with each side's count), never compared by one of its entries.
pub fn compare(a: &[Schedule], b: &[Schedule], o: &Opts) -> Vec<Parity> {
    let index = |v: &[Schedule]| {
        let mut m: BTreeMap<String, Vec<Schedule>> = BTreeMap::new();
        for s in v {
            m.entry(s.name.clone()).or_default().push(s.clone());
        }
        m
    };
    let (ia, ib) = (index(a), index(b));
    let names: BTreeSet<&String> = ia.keys().chain(ib.keys()).collect();
    names
        .into_iter()
        .map(|n| {
            let one = |status, a: String, b: String| Parity {
                name: n.clone(),
                status,
                a,
                b,
                expected: false,
            };
            let (ca, cb) = (ia.get(n).map_or(0, Vec::len), ib.get(n).map_or(0, Vec::len));
            if ca > 1 || cb > 1 {
                return one(Status::DuplicateName, ca.to_string(), cb.to_string());
            }
            match (ia.get(n).map(|v| &v[0]), ib.get(n).map(|v| &v[0])) {
                (Some(x), Some(y)) => pair(n, x, y, o),
                (Some(x), None) if x.kind == ScheduleKind::Event => {
                    one(Status::EventOnly, "event".into(), String::new())
                }
                (Some(x), None) => one(Status::OnlyA, when(x), String::new()),
                (None, Some(y)) if y.kind == ScheduleKind::Event => {
                    one(Status::EventOnly, String::new(), "event".into())
                }
                (None, Some(y)) => one(Status::OnlyB, String::new(), when(y)),
                (None, None) => unreachable!("every name comes from one side"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize_schedule(s)
    }

    fn sched(name: &str, cron: Option<&str>, tz: &str, prompt: &str, enabled: bool) -> Schedule {
        Schedule {
            name: name.into(),
            cron: cron.map(String::from),
            tz: tz.into(),
            kind: if cron.is_some() {
                ScheduleKind::Scheduled
            } else {
                ScheduleKind::Event
            },
            action: prompt.into(),
            enabled,
        }
    }

    fn opts() -> Opts {
        Opts {
            default_tz: "UTC".into(),
            ..Opts::default()
        }
    }

    fn one(a: Vec<Schedule>, b: Vec<Schedule>, o: &Opts) -> Parity {
        let r = compare(&a, &b, o);
        assert_eq!(r.len(), 1, "{r:?}");
        r[0].clone()
    }

    const P: &str = "Example prompt: summarise the demo log.";

    #[test]
    fn cron_whitespace_is_collapsed() {
        assert_eq!(n("  15\t5  *   * * "), "15 5 * * *");
        assert_eq!(n("*/15 * * * *"), "*/15 * * * *");
    }

    #[test]
    fn day_and_month_names_become_numbers() {
        assert_eq!(n("0 9 * * MON-FRI"), "0 9 * * 1-5");
        assert_eq!(n("0 9 * * mon,Wed,FRI"), "0 9 * * 1,3,5");
        assert_eq!(n("0 0 1 JAN,jul *"), "0 0 1 1,7 *");
        assert_eq!(n("0 0 1 mar-may *"), "0 0 1 3-5 *");
        assert_eq!(n("0 9 * * sun-sat/2"), "0 9 * * 0-6/2");
    }

    #[test]
    fn sunday_is_zero_and_seven() {
        assert_eq!(n("0 9 * * 7"), n("0 9 * * 0"));
        assert_eq!(n("0 9 * * SUN"), "0 9 * * 0");
        assert_eq!(n("0 9 * * 5-7"), "0 9 * * 0,5,6");
        assert_eq!(n("0 9 * * 6,0"), n("0 9 * * sat,7"));
    }

    #[test]
    fn value_lists_are_sorted_and_leading_zeros_dropped() {
        assert_eq!(n("15,0 05 * * *"), "0,15 5 * * *");
        assert_eq!(n("0 9 * * 1,2,3"), "0 9 * * 1-3");
        assert_eq!(n("0 9 * * 3,1"), "0 9 * * 1,3");
        assert_eq!(n("0 9 * * 1-5"), "0 9 * * 1-5");
        assert_eq!(n("05/15 * * * *"), "5/15 * * * *");
    }

    #[test]
    fn intervals_that_cron_expresses_become_cron() {
        assert_eq!(n("every 1d at 05:15"), "15 5 * * *");
        assert_eq!(n("every 1d at 5:15"), "15 5 * * *");
        assert_eq!(n("every 6h"), "0 */6 * * *");
        assert_eq!(n("every 15m"), "*/15 * * * *");
        assert_eq!(n("Every 15 minutes"), "*/15 * * * *");
        assert_eq!(n("every 360m"), "0 */6 * * *");
        assert_eq!(n("every 1h"), "0 * * * *");
        assert_eq!(n("every hour"), "0 * * * *");
        assert_eq!(n("every 1m"), "* * * * *");
        assert_eq!(n("every 1440m"), "every 1440m");
        assert_eq!(n("every 1d"), "every 1440m");
    }

    #[test]
    fn other_intervals_and_other_text_stay_as_written() {
        assert_eq!(n("every 90m"), "every 90m");
        assert_eq!(n("every 90 minutes"), "every 90m");
        assert_eq!(n("every 7h"), "every 420m");
        assert_eq!(n("every 2d"), "every 2880m");
        assert_eq!(n("every 2d at 05:15"), "every 2d at 05:15");
        assert_eq!(n("every 1d at noon"), "every 1d at noon");
        assert_eq!(n("every  other   week"), "every other week");
        assert_eq!(n("0 9 * * * *"), "0 9 * * * *");
        assert_eq!(n("60 * * * *"), "60 * * * *");
        assert_eq!(n("0 9 L * *"), "0 9 L * *");
        assert_eq!(n("every 0m"), "every 0m");
    }

    #[test]
    fn empty_timezone_is_the_default_and_utc_has_one_spelling() {
        assert_eq!(normalize_tz("", "Etc/GMT+3"), "Etc/GMT+3");
        assert_eq!(normalize_tz("  ", "UTC"), "UTC");
        assert_eq!(normalize_tz("Etc/UTC", "Etc/GMT+3"), "UTC");
        assert_eq!(normalize_tz("utc", "Etc/GMT+3"), "UTC");
        assert_eq!(normalize_tz(" Etc/GMT+3 ", "UTC"), "Etc/GMT+3");
        assert_eq!(shown_tz("Europe/Berlin"), "Europe/Berlin");
        assert_eq!(shown_tz("Etc/GMT+3"), "Etc/GMT+3");
        assert!(shown_tz("x\u{1b}[2J").starts_with("(a zone"));
        assert!(shown_tz("\u{202e}UTC").starts_with("(a zone"));
    }

    #[test]
    fn prompts_compare_without_line_ending_or_trailing_space_noise() {
        assert_eq!(normalize_prompt("a  \r\nb\n\n"), "a\nb");
        assert_eq!(prompt_digest("a\r\nb "), prompt_digest("a\nb"));
        assert_ne!(prompt_digest("a"), prompt_digest("b"));
        assert_eq!(prompt_digest(P).len(), 8);
    }

    #[test]
    fn status_same_across_spellings() {
        let a = sched("demo-routine", Some("every 1d at 05:15"), "", P, true);
        let b = sched(
            "demo-routine",
            Some("15 05 * * *"),
            "Etc/UTC",
            "Example prompt: summarise the demo log.\n",
            true,
        );
        let r = one(vec![a], vec![b], &opts());
        assert_eq!(r.status, Status::Same);
        assert!(r.ok());
    }

    #[test]
    fn status_schedule_differs() {
        let a = sched("demo-routine", Some("0 8 * * *"), "UTC", P, true);
        let b = sched("demo-routine", Some("every 6h"), "UTC", "other", false);
        let r = one(vec![a.clone()], vec![b], &opts());
        assert_eq!(
            (r.status, r.a.as_str(), r.b.as_str()),
            (Status::ScheduleDiffers, "0 8 * * *", "0 */6 * * *")
        );
        assert!(!r.ok());
        // an event on one side and a schedule on the other is a schedule difference
        let e = sched("demo-routine", None, "UTC", P, true);
        let r = one(vec![e], vec![a], &opts());
        assert_eq!((r.status, r.a.as_str()), (Status::ScheduleDiffers, "event"));
    }

    #[test]
    fn status_tz_differs() {
        let a = sched("demo-routine", Some("0 8 * * *"), "Etc/GMT+3", P, true);
        let b = sched("demo-routine", Some("0 8 * * *"), "", P, true);
        let r = one(vec![a], vec![b.clone()], &opts());
        assert_eq!(
            (r.status, r.a.as_str(), r.b.as_str()),
            (Status::TzDiffers, "Etc/GMT+3", "UTC")
        );
        // the default timezone is what an empty one means
        let o = Opts {
            default_tz: "Etc/GMT+3".into(),
            ..opts()
        };
        let a = sched("demo-routine", Some("0 8 * * *"), "Etc/GMT+3", P, true);
        assert_eq!(one(vec![a], vec![b], &o).status, Status::Same);
    }

    #[test]
    fn status_prompt_differs_shows_digests_not_text() {
        let a = sched("demo-routine", Some("0 8 * * *"), "UTC", P, true);
        let b = sched(
            "demo-routine",
            Some("0 8 * * *"),
            "UTC",
            "Example prompt: count the demo rows.",
            false,
        );
        let r = one(vec![a], vec![b], &opts());
        assert_eq!(r.status, Status::PromptDiffers);
        assert_eq!(r.a, prompt_digest(P));
        assert!(!r.a.contains("Example") && !r.b.contains("Example"));
        assert_eq!(r.b.len(), 8);
    }

    #[test]
    fn status_paused_differs_expected_only_for_the_policy_paused_side() {
        let a = sched("demo-routine", Some("0 8 * * *"), "UTC", P, true);
        let b = sched("demo-routine", Some("0 8 * * *"), "UTC", P, false);
        let r = one(vec![a.clone()], vec![b.clone()], &opts());
        assert_eq!(
            (r.status, r.a.as_str(), r.b.as_str(), r.expected),
            (Status::PausedDiffers, "enabled", "paused", false)
        );
        assert!(!r.ok());
        let mut o = opts();
        o.paused_by_policy_b.insert("demo-routine".into());
        let r = one(vec![a.clone()], vec![b.clone()], &o);
        assert!(r.expected && r.ok());
        // the policy set of the side that is enabled does not explain it
        let mut o = opts();
        o.paused_by_policy_a.insert("demo-routine".into());
        assert!(!one(vec![a], vec![b], &o).ok());
    }

    #[test]
    fn status_only_a_only_b_and_event_only() {
        let s = sched("demo-routine", Some("0 8 * * *"), "UTC", P, true);
        let e = sched("demo-event", None, "UTC", P, true);
        let r = one(vec![s.clone()], vec![], &opts());
        assert_eq!((r.status, r.a.as_str()), (Status::OnlyA, "0 8 * * *"));
        assert!(!r.ok());
        let r = one(vec![], vec![s], &opts());
        assert_eq!((r.status, r.b.as_str()), (Status::OnlyB, "0 8 * * *"));
        let r = one(vec![e.clone()], vec![], &opts());
        assert_eq!(r.status, Status::EventOnly);
        assert!(r.ok());
        assert_eq!(
            one(vec![], vec![e.clone()], &opts()).status,
            Status::EventOnly
        );
        // an event on both sides compares like any routine (no timezone)
        let mut e2 = e.clone();
        e2.tz = "Etc/GMT+3".into();
        assert_eq!(one(vec![e], vec![e2], &opts()).status, Status::Same);
    }

    #[test]
    fn compare_is_sorted_by_name_and_reports_a_duplicate_name() {
        let x = |n: &str, c: &str| sched(n, Some(c), "UTC", P, true);
        let r = compare(
            &[
                x("demo-b", "0 1 * * *"),
                x("demo-a", "0 1 * * *"),
                x("demo-a", "0 2 * * *"),
            ],
            &[x("demo-a", "0 1 * * *"), x("demo-c", "0 1 * * *")],
            &opts(),
        );
        let got: Vec<(&str, &str)> = r
            .iter()
            .map(|p| (p.name.as_str(), p.status.word()))
            .collect();
        assert_eq!(
            got,
            [
                ("demo-a", "duplicate-name"),
                ("demo-b", "only-a"),
                ("demo-c", "only-b")
            ]
        );
        // each side's count; neither entry of demo-a is compared
        assert_eq!((r[0].a.as_str(), r[0].b.as_str()), ("2", "1"));
        assert!(!r[0].ok());
    }

    #[test]
    fn neutral_form_is_normalised_and_can_leave_out_enabled() {
        let a = sched("demo-routine", Some("every 1d at 05:15"), "", P, true);
        let b = sched(
            "demo-routine",
            Some("15 5 * * *"),
            "UTC",
            &format!("{P}  \r\n"),
            false,
        );
        assert_eq!(
            serde_json::to_string(&neutral(&a, "UTC", false)).unwrap(),
            r#"{"enabled":true,"kind":"scheduled","prompt":"Example prompt: summarise the demo log.","schedule":"15 5 * * *","tz":"UTC"}"#
        );
        assert_ne!(
            neutral_digest(&a, "UTC", false),
            neutral_digest(&b, "UTC", false)
        );
        assert_eq!(
            neutral_digest(&a, "UTC", true),
            neutral_digest(&b, "UTC", true)
        );
        assert_eq!(neutral_digest(&a, "UTC", true).len(), 64);
        let e = sched("demo-event", None, "Etc/GMT+3", P, true);
        assert_eq!(
            neutral(&e, "UTC", true)["schedule"],
            serde_json::Value::Null
        );
        assert_eq!(neutral(&e, "UTC", true)["tz"], "");
    }
}

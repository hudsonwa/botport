//! The schedule strings Hermes accepts, mirrored from upstream hermes-agent `cron/jobs.py`: `parse_schedule`
//! (with `_natural_every_to_cron`, `_parse_clock_time`, `parse_duration` and croniter's field ranges).
//! botport never turns other English into cron: a string outside this grammar is reported, not guessed.
//!
//! Accepted, in Hermes' order:
//! - a day/time phrase, with or without `every `: `every monday 9am`, `weekdays at 9am`, `daily 07:00`,
//!   `every mon, wed and fri at noon` -> cron
//! - `every <duration>` or a bare duration: `every 30m`, `2h`, `every hour` -> interval (minutes)
//! - a 5-field cron expression (numbers, `*`, `-`, `,`, `/`, JAN-DEC, SUN-SAT) -> cron
//! - an ISO timestamp or `in <duration>` -> one-shot (a routine repeats, so botport does not import these)
//!
//! Stricter than Hermes on purpose (all go to the manual checklist):
//! - cron must have exactly 5 fields (croniter reads a 6th as seconds);
//! - croniter extensions are refused: `L`, `R` and wrap-around ranges (`10-5`, `fri-mon`); `#` and `?` Hermes
//!   rejects itself;
//! - a day of month no listed month has (`0 0 30 2 *`): croniter finds no next run and `hermes cron create` fails;
//! - an interval of 0 minutes, or over [`MAX_MINUTES`] (Hermes' `now + timedelta` overflows on huge values);
//! - a timestamp is a one-shot only in the `datetime.fromisoformat` forms botport checks
//!   (`YYYY-MM-DD[(T| )HH:MM[:SS[.f]]][Z|±HH:MM]`); other text with a `T` or a leading date Hermes rejects too.

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Cron(String),
    Interval(u32),
    Once,
}

/// The longest interval botport accepts: one year.
pub const MAX_MINUTES: u32 = 525_600;

fn duration(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_lowercase();
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    let unit = s[digits..].trim_start();
    let n: u32 = if digits == 0 {
        1
    } else {
        s[..digits].parse().ok()?
    };
    let mult = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 1,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60,
        "d" | "day" | "days" => 1440,
        _ => return None,
    };
    n.checked_mul(mult)
}

fn clock(t: &str) -> Option<(u32, u32)> {
    let t: String = t
        .to_ascii_lowercase()
        .chars()
        .filter(|c| *c != ' ')
        .collect();
    match t.as_str() {
        "" => return None,
        "noon" | "midday" => return Some((12, 0)),
        "midnight" => return Some((0, 0)),
        _ => {}
    }
    let (t, mer) = match t.strip_suffix("am").or_else(|| t.strip_suffix("pm")) {
        Some(x) => (x, Some(t.ends_with("pm"))),
        None => (t.as_str(), None),
    };
    let (h, m) = t.split_once(':').unwrap_or((t, "00"));
    let ok = |x: &str, w: std::ops::RangeInclusive<usize>| {
        w.contains(&x.len()) && x.bytes().all(|b| b.is_ascii_digit())
    };
    if !ok(h, 1..=2) || !ok(m, 2..=2) {
        return None;
    }
    let (mut h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    if let Some(pm) = mer {
        if !(1..=12).contains(&h) {
            return None;
        }
        h = h % 12 + if pm { 12 } else { 0 };
    }
    (h <= 23 && m <= 59).then_some((h, m))
}

fn weekday(t: &str) -> Option<&'static str> {
    Some(match t {
        "sunday" | "sun" => "0",
        "monday" | "mon" => "1",
        "tuesday" | "tue" | "tues" => "2",
        "wednesday" | "wed" | "weds" => "3",
        "thursday" | "thu" | "thur" | "thurs" => "4",
        "friday" | "fri" => "5",
        "saturday" | "sat" => "6",
        _ => return None,
    })
}

/// `_natural_every_to_cron`: `<days> [at] <time>` -> 5-field cron.
fn natural(rest: &str) -> Option<String> {
    let rest = rest.to_ascii_lowercase().replace(',', " ");
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let first = *tokens.first()?;
    let (dow, idx) = match first {
        "day" | "daily" | "everyday" => ("*".to_string(), 1),
        "weekday" | "weekdays" => ("1-5".to_string(), 1),
        "weekend" | "weekends" => ("0,6".to_string(), 1),
        _ => {
            let mut days: Vec<&str> = vec![];
            let mut idx = tokens.len();
            for (i, t) in tokens.iter().enumerate() {
                if *t == "and" {
                    continue;
                }
                match weekday(t) {
                    Some(d) if !days.contains(&d) => days.push(d),
                    Some(_) => {}
                    None => {
                        idx = i;
                        break;
                    }
                }
            }
            if days.is_empty() {
                return None;
            }
            (days.join(","), idx)
        }
    };
    let mut time = &tokens[idx..];
    if time.first() == Some(&"at") {
        time = &time[1..];
    }
    if time.is_empty() {
        return None;
    }
    let (h, m) = clock(&time.join(" "))?;
    Some(format!("{m} {h} * * {dow}"))
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn cron_value(v: &str, field: usize) -> Option<u32> {
    if v.bytes().all(|b| b.is_ascii_digit()) && !v.is_empty() {
        return v.parse().ok();
    }
    let l = v.to_ascii_lowercase();
    match field {
        3 => MONTHS.iter().position(|x| *x == l).map(|i| i as u32 + 1),
        4 => DAYS.iter().position(|x| *x == l).map(|i| i as u32),
        _ => None,
    }
}

/// croniter's ranges: minute 0-59, hour 0-23, day 1-31, month 1-12, weekday 0-7.
fn cron_field(f: &str, field: usize) -> bool {
    let (lo, hi) = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)][field];
    f.split(',').all(|part| {
        let base = match part.split_once('/') {
            Some((b, s))
                if !s.is_empty()
                    && s.bytes().all(|c| c.is_ascii_digit())
                    && s.parse::<u32>().is_ok_and(|s| s >= 1) =>
            {
                b
            }
            Some(_) => return false,
            None => part,
        };
        if base == "*" {
            return true;
        }
        let (a, b) = base.split_once('-').unwrap_or((base, base));
        match (cron_value(a, field), cron_value(b, field)) {
            (Some(a), Some(b)) => lo <= a && a <= b && b <= hi,
            _ => false,
        }
    })
}

/// The values a valid field lists (`*` -> the whole range).
fn cron_values(f: &str, field: usize) -> Vec<u32> {
    let (lo, hi) = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)][field];
    let mut out = vec![];
    for part in f.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => (b, s.parse().unwrap_or(1)),
            None => (part, 1),
        };
        let (a, b) = match base {
            "*" => (lo, hi),
            _ => {
                let (a, b) = base.split_once('-').unwrap_or((base, base));
                let a = cron_value(a, field).unwrap_or(lo);
                // croniter reads `5/15` as `5-max/15`
                let b = match (base.contains('-'), part.contains('/')) {
                    (false, true) => hi,
                    _ => cron_value(b, field).unwrap_or(a),
                };
                (a, b)
            }
        };
        out.extend((a..=b).step_by(step as usize));
    }
    out
}

fn cron(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 5 || !parts.iter().enumerate().all(|(i, f)| cron_field(f, i)) {
        return None;
    }
    // some listed month must have a listed day: croniter 6 first computes the day-of-month half even when a weekday
    // is set, and raises "failed to find next date" if no month has that day, so `hermes cron create` would fail
    const DAYS_IN: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let first_day = *cron_values(parts[2], 2).iter().min()?;
    if !cron_values(parts[3], 3)
        .iter()
        .any(|m| first_day <= DAYS_IN[*m as usize - 1])
    {
        return None;
    }
    Some(parts.join(" "))
}

fn digits(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(u8::is_ascii_digit)
}

/// The `datetime.fromisoformat` forms botport recognises: a real date, an optional time and an optional offset.
fn valid_iso(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 10
        || !digits(&b[..4])
        || b[4] != b'-'
        || !digits(&b[5..7])
        || b[7] != b'-'
        || !digits(&b[8..10])
    {
        return false;
    }
    let (y, mo, d): (u32, u32, u32) = (
        s[..4].parse().unwrap(),
        s[5..7].parse().unwrap(),
        s[8..10].parse().unwrap(),
    );
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let max = match mo {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return false,
    };
    if y == 0 || d == 0 || d > max {
        return false;
    }
    let rest = &s[10..];
    if rest.is_empty() {
        return true;
    }
    let Some(t) = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' ')) else {
        return false;
    };
    let (t, off) = match t.find(['Z', '+', '-']) {
        Some(i) => t.split_at(i),
        None => (t, ""),
    };
    let two = |x: &str, max: u32| {
        x.len() == 2 && digits(x.as_bytes()) && x.parse::<u32>().is_ok_and(|v| v <= max)
    };
    let off_ok = match off {
        "" | "Z" => true,
        o => o[1..]
            .split_once(':')
            .is_some_and(|(h, m)| two(h, 23) && two(m, 59)),
    };
    let (hms, frac) = t.split_once('.').unwrap_or((t, "0"));
    let f: Vec<&str> = hms.split(':').collect();
    off_ok
        && digits(frac.as_bytes())
        && (2..=3).contains(&f.len())
        && two(f[0], 23)
        && two(f[1], 59)
        && f.get(2).is_none_or(|x| two(x, 59))
}

/// Text Hermes sends down its timestamp branch: it contains `T` or starts with `dddd-dd-dd`.
fn iso_like(s: &str) -> bool {
    let b = s.as_bytes();
    s.contains('T')
        || (b.len() >= 10
            && digits(&b[..4])
            && b[4] == b'-'
            && digits(&b[5..7])
            && b[7] == b'-'
            && digits(&b[8..10]))
}

/// `parse_schedule`: what Hermes makes of `s`, or `None` when Hermes would reject it.
pub fn parse(s: &str) -> Option<Parsed> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let every = lower.strip_prefix("every ").map(str::trim);
    if let Some(c) = natural(every.unwrap_or(&lower)) {
        return Some(Parsed::Cron(c));
    }
    if let Some(rest) = every {
        return duration(rest).map(Parsed::Interval);
    }
    if s.split_whitespace().count() >= 5 {
        return cron(s).map(Parsed::Cron);
    }
    if iso_like(s) {
        // Hermes raises on a timestamp `fromisoformat` rejects; it never falls through to a duration
        return valid_iso(s).then_some(Parsed::Once);
    }
    if let Some(d) = lower.strip_prefix("in ") {
        return duration(d).map(|_| Parsed::Once);
    }
    duration(s).map(Parsed::Interval)
}

/// The schedule botport hands to `hermes cron create`, in the form Hermes stores and `cron_expr` reads back:
/// a cron expression, or `every Nm` for an interval. `Err` is the reason for the manual checklist.
pub fn normalize(s: &str) -> Result<String, String> {
    // any run of whitespace (tabs, NBSP) becomes one space: Hermes gets the same schedule in its plain-space form
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match parse(&one) {
        Some(Parsed::Cron(c)) => Ok(c),
        Some(Parsed::Interval(m)) if (1..=MAX_MINUTES).contains(&m) => Ok(format!("every {m}m")),
        Some(Parsed::Once) => Err(format!(
            "not imported: '{one}' runs once, not on a schedule; set it by hand"
        )),
        _ => Err(format!(
            "not imported: '{one}' is not a schedule Hermes accepts; set it by hand"
        )),
    }
}

/// The `schedule` object Hermes stores in cron/jobs.json for a normalized schedule.
pub fn stored(s: &str) -> serde_json::Value {
    match s
        .strip_prefix("every ")
        .and_then(|m| m.strip_suffix('m'))
        .and_then(|m| m.parse::<u32>().ok())
    {
        Some(m) => serde_json::json!({"kind": "interval", "minutes": m, "display": s}),
        None => serde_json::json!({"kind": "cron", "expr": s}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Result<String, String> {
        normalize(s)
    }

    #[test]
    fn day_time_phrases_with_and_without_every() {
        assert_eq!(n("every monday 9am"), Ok("0 9 * * 1".into()));
        assert_eq!(n("weekdays at 9am"), Ok("0 9 * * 1-5".into()));
        assert_eq!(n("daily 06:10"), Ok("10 6 * * *".into()));
        assert_eq!(n("every day at 7:30pm"), Ok("30 19 * * *".into()));
        assert_eq!(n("Every Weekend noon"), Ok("0 12 * * 0,6".into()));
        assert_eq!(n("everyday midnight"), Ok("0 0 * * *".into()));
        assert_eq!(
            n("every mon, wed and fri at 9 am"),
            Ok("0 9 * * 1,3,5".into())
        );
        assert_eq!(n("every friday 16:00"), Ok("0 16 * * 5".into()));
        assert_eq!(n("tue tues 7"), Ok("0 7 * * 2".into())); // bare 24h hour, duplicate day once
        assert_eq!(n("weekday 12am"), Ok("0 0 * * 1-5".into()));
    }

    #[test]
    fn stored_form_reads_back_through_cron_expr() {
        for s in ["every 15m", "0 9 * * 1-5"] {
            let j = serde_json::json!({"schedule": stored(s)});
            assert_eq!(super::super::sync::cron_expr(&j), s);
        }
    }

    #[test]
    fn intervals() {
        assert_eq!(n("every 30m"), Ok("every 30m".into()));
        assert_eq!(n("every 15 min"), Ok("every 15m".into()));
        assert_eq!(n("every 2 hours"), Ok("every 120m".into()));
        assert_eq!(n("every hour"), Ok("every 60m".into()));
        assert_eq!(n("every day"), Ok("every 1440m".into())); // "day" with no time is a duration upstream too
        assert_eq!(n("90m"), Ok("every 90m".into()));
        assert_eq!(n("1d"), Ok("every 1440m".into()));
    }

    #[test]
    fn cron_expressions() {
        assert_eq!(n("0 9 * * *"), Ok("0 9 * * *".into()));
        assert_eq!(
            n("  */15  *  * * MON-FRI "),
            Ok("*/15 * * * MON-FRI".into())
        );
        assert_eq!(n("0 0 1 jan,jul 0"), Ok("0 0 1 jan,jul 0".into()));
        assert_eq!(n("5-10/2 1,2 * * 7"), Ok("5-10/2 1,2 * * 7".into()));
        for bad in [
            "60 * * * *",
            "0 24 * * *",
            "0 0 0 * *",
            "0 0 * 13 *",
            "0 0 * * 8",
            "*/0 * * * *",
            "0 9 * * * *",
            "0 9 L * *",
            "0 9 * * 5#2",
            "10-5 * * * *",
            "0 9 ? * *",
            "a b c d e",
        ] {
            assert!(n(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn one_shots_are_recognised_but_not_imported() {
        assert_eq!(parse("2000-02-03T14:00:00"), Some(Parsed::Once));
        assert_eq!(parse("in 30m"), Some(Parsed::Once));
        assert!(n("in 2h").unwrap_err().contains("runs once"));
        for once in [
            "1999-02-29T09:00",
            "2000-02-29",
            "2000-01-02 03:15:00.5+03:00",
            "2000-01-02T03:15Z",
        ] {
            assert_eq!(
                parse(once),
                (!once.starts_with("1999-02")).then_some(Parsed::Once),
                "{once}"
            );
        }
        // Hermes raises "Invalid timestamp" on these: not a one-shot, not a schedule
        for bad in [
            "Thursdays 9am",
            "Twice a day",
            "Tea time",
            "2000-13-45",
            "2000-04-31",
            "2000-01-02T25:00",
        ] {
            assert_eq!(
                n(bad).unwrap_err(),
                format!("not imported: '{bad}' is not a schedule Hermes accepts; set it by hand")
            );
        }
    }

    #[test]
    fn crons_with_no_next_run_and_huge_intervals_are_refused() {
        for bad in [
            "0 0 30 2 *",
            "0 0 31 4 *",
            "0 0 31 2,4,6 *",
            "0 0 30-31 feb *",
            "0 0 30 2 1",
            "0 0 31 4 sun",
        ] {
            assert!(n(bad).is_err(), "{bad}");
        }
        for ok in [
            "0 0 29 2 *",
            "0 0 31 1-12 *",
            "0 0 1 * 1",
            "0 0 */10 2 *",
            "0 0 31 2,3 *",
        ] {
            assert_eq!(n(ok), Ok(ok.into()), "{ok}");
        }
        assert_eq!(n("every 365d"), Ok("every 525600m".into()));
        for bad in [
            "every 366d",
            "every 4294967295m",
            "every 99999999999m",
            "every 0m",
        ] {
            assert!(n(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn english_that_hermes_rejects_is_never_guessed() {
        for bad in [
            "hourly",
            "hourly at :20",
            "nightly 2am",
            "fridays 16:00",
            "7am weekdays",
            "every weekday",
            "daily",
            "every other tuesday 9am",
            "first monday of the month 9am",
            "whenever it feels right",
            "every 15 minutes or so",
            "daily at 25:00",
            "daily at 13pm",
            "twice a day",
            "every 0m",
            "",
        ] {
            let e = n(bad).unwrap_err();
            assert!(e.contains("is not a schedule Hermes accepts"), "{bad}: {e}");
        }
    }
}

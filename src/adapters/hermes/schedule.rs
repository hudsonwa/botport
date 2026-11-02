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
//! Stricter than Hermes on purpose: cron must have exactly 5 fields (croniter reads a 6th as seconds) and
//! croniter extensions (`L`, `#`, `?`, wrap-around ranges) are refused; both go to the manual checklist.

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Cron(String),
    Interval(u32),
    Once,
}

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

fn cron(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    (parts.len() == 5 && parts.iter().enumerate().all(|(i, f)| cron_field(f, i)))
        .then(|| parts.join(" "))
}

fn iso(s: &str) -> bool {
    let b = s.as_bytes();
    s.contains('T')
        || (b.len() >= 10 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-' && b[7] == b'-')
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
    if iso(s)
        || lower
            .strip_prefix("in ")
            .is_some_and(|d| duration(d).is_some())
    {
        return Some(Parsed::Once);
    }
    duration(s).map(Parsed::Interval)
}

/// The schedule botport hands to `hermes cron create`, in the form Hermes stores and `cron_expr` reads back:
/// a cron expression, or `every Nm` for an interval. `Err` is the reason for the manual checklist.
pub fn normalize(s: &str) -> Result<String, String> {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match parse(&one) {
        Some(Parsed::Cron(c)) => Ok(c),
        Some(Parsed::Interval(m)) if m >= 1 => Ok(format!("every {m}m")),
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
        assert_eq!(parse("2026-02-03T14:00:00"), Some(Parsed::Once));
        assert_eq!(parse("in 30m"), Some(Parsed::Once));
        assert!(n("in 2h").unwrap_err().contains("runs once"));
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

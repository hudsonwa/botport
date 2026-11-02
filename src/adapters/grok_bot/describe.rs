//! Parse the bot's own description (NOT_CAPTURED.md or a --describe file) into describe.json.
//! Free text is tolerated, never guessed at: whatever can't be classified lands in `unparsed` with its line number.
//! Text is never rewritten. A schedule or connector line goes through the routing rule [`secrets::scan`], any other
//! line through the write guard [`secrets::guard_str`]: a clean line is copied byte-identical, a
//! suspicious schedule or connector goes to `manual` (a display name, its line and the reason, none of its text), and
//! any other suspicious text becomes a marker. Credentials keep names only.
use crate::core::secrets::{self, Reason};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

pub const SCHEMA: u32 = 1;
const REDACTED: &str = "[line with a secret value removed]";
/// A credential line with no identifier-shaped name: its text may hold a value, so none of it is kept.
const VALUE_NAME: &str = "[credential name may contain a value; text not copied]";
const NO_NAME: &str = "[credential line without a NAME, `name` or **name**; text not copied]";

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

#[derive(Clone, Copy, PartialEq)]
enum Sec {
    Pre,
    Instructions,
    Schedules,
    Connectors,
    Credentials,
    Other,
}

fn section_of(h: &str) -> Sec {
    let h = h.to_ascii_lowercase();
    if h.contains("instruction") || h.contains("system prompt") {
        Sec::Instructions
    } else if h.contains("schedule") || h.contains("automation") || h.contains("routine") {
        Sec::Schedules
    } else if h.contains("connector") || h.contains("integration") || h.contains("mcp") {
        Sec::Connectors
    } else if h.contains("credential") || h.contains("secret") {
        Sec::Credentials
    } else {
        Sec::Other
    }
}

/// Credential names and locations name a secret by design: only a value makes them suspicious.
fn is_secret(s: &str) -> bool {
    secrets::scan_value(s).is_some()
}

fn not_copied(r: Reason) -> String {
    format!("[text not copied: {r}]")
}

/// "- **Name**: rest", "- `Name` — rest", "- Name: rest" -> (name, rest). List markers are stripped first.
fn split_item(line: &str) -> (Option<String>, String) {
    static L: OnceLock<Regex> = OnceLock::new();
    static N: OnceLock<Regex> = OnceLock::new();
    let t = re(&L, r"^\s*(?:[-*+]|\d+[.)])\s+")
        .replace(line, "")
        .trim()
        .to_string();
    let n = re(
        &N,
        r"^(?:\*\*(.+?)\*\*|`(.+?)`|([^:—–]{1,60}?))\s*(?:[:—–]\s+|\s+-\s+|$)(.*)$",
    );
    match n.captures(&t) {
        Some(c) => {
            let name = c
                .get(1)
                .or(c.get(2))
                .or(c.get(3))
                .map(|m| m.as_str().trim().trim_end_matches(':').to_string());
            let rest = c.get(4).map_or("", |m| m.as_str()).trim().to_string();
            // A bare "Name" with no separator is only a name when it was bold/backticked.
            if c.get(3).is_some() && rest.is_empty() && !t.ends_with(':') {
                (None, t)
            } else {
                (name, rest)
            }
        }
        None => (None, t),
    }
}

/// A tool name is identifier-shaped and not a value.
fn tool_name(s: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^[A-Za-z0-9_.\-]+$").is_match(s) && !is_secret(s)
}

fn none_line(t: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)^\s*(?:[-*]\s+)?(?:none|no active routines|no schedules|no connectors|no credentials|n/?a)\b").is_match(t)
}

/// `Some(Err(()))` = an ambiguous abbreviation (CST): reported, never guessed.
fn timezone(t: &str) -> Option<Result<String, ()>> {
    timezone_at(t).map(|(z, _)| z)
}

/// The timezone and the byte range of its token in `t`.
fn timezone_at(t: &str) -> Option<(Result<String, ()>, std::ops::Range<usize>)> {
    static I: OnceLock<Regex> = OnceLock::new();
    static A: OnceLock<Regex> = OnceLock::new();
    if let Some(m) = re(&I, r"\b(?:Africa|America|Antarctica|Asia|Atlantic|Australia|Europe|Indian|Pacific|Etc)/[A-Za-z_]+(?:/[A-Za-z_]+)?\b").find(t) {
        return Some((Ok(m.as_str().into()), m.range()));
    }
    let a = re(
        &A,
        // Only a whole token right after a time ("7am PST", "07:00Z", "every 15 min UTC") or "at" ("at UTC"), never prose ("Plan Z").
        r"(?:\d(?:\s*[aApP][mM])?\s*|\b[aA][tT]\s+|\d\s*(?:mins?|minutes?|hours?|hrs?)\s+)(UTC|GMT|Z|AWST|AEST|AEDT|ACST|ACDT|JST|SGT|CET|CEST|BST|EST|EDT|CST|CDT|PST|PDT)\b",
    )
    .captures(t)?;
    let at = a.get(1).unwrap().range();
    if &a[1] == "CST" {
        return Some((Err(()), at));
    }
    Some((
        Ok(match &a[1] {
            "UTC" | "GMT" | "Z" => "UTC",
            "AWST" => "Etc/GMT-8",
            "AEST" | "AEDT" => "Australia/Sydney",
            "ACST" | "ACDT" => "Australia/Adelaide",
            "JST" => "Asia/Tokyo",
            "SGT" => "Asia/Singapore",
            "CET" | "CEST" => "Europe/Paris",
            "BST" => "Europe/London",
            "EST" | "EDT" => "America/New_York",
            "CDT" => "America/Chicago",
            _ => "America/Los_Angeles",
        }
        .into()),
        at,
    ))
}

/// The schedule text of a `when`: the timezone token, "(paused)" and backticks around a cron literal removed.
/// What is left must be a string Hermes accepts (`hermes::schedule`); nothing else is interpreted. An ambiguous
/// timezone stays in, so the schedule is refused rather than run in a guessed zone.
fn schedule_text(when: &str) -> String {
    let mut t = when.to_string();
    if let Some((Ok(_), r)) = timezone_at(&t) {
        t.replace_range(r, " ");
    }
    static P: OnceLock<Regex> = OnceLock::new();
    let t = re(&P, r"(?i)\(paused\)")
        .replace_all(&t, " ")
        .replace('`', " ");
    let mut t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    for tail in [" at", " in"] {
        if t.to_ascii_lowercase().ends_with(tail) {
            t.truncate(t.len() - tail.len());
        }
    }
    t
}

fn other_side(t: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    static S: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)\b(?:another|other|different|separate)\s+(?:machine|host|server|computer|box)\b|\bnot\s+on\s+grok\b|\boutside\s+grok\b").is_match(t)
        // "Web side", "laptop side": a side that is not the Grok one
        || re(&S, r"(?i)\b(\w+)\s+side\b")
            .captures_iter(t)
            .any(|c| !["grok", "bot", "this", "our", "the"].contains(&c[1].to_ascii_lowercase().as_str()))
}

/// A lead-in that puts the lines after it on another side, until the next heading or blank line:
/// "None on the Grok side …", "Laptop side, for context:".
fn context_lead_in(t: &str) -> bool {
    static L: OnceLock<Regex> = OnceLock::new();
    static C: OnceLock<Regex> = OnceLock::new();
    !re(&L, r"^\s*(?:[-*+]|\d+[.)])\s").is_match(t)
        && (none_line(t)
            || re(&C, r"(?i)\bfor context\b").is_match(t)
            || (t.ends_with(':') && other_side(t)))
}

fn prompt_of(t: &str) -> (String, Option<String>) {
    static R: OnceLock<Regex> = OnceLock::new();
    match re(&R, r#"(?i)\s*[—–-]?\s*\bprompt\s*:\s*"?([^"]*)"?\s*$"#).captures(t) {
        Some(c) => (
            t[..c.get(0).unwrap().start()].trim().to_string(),
            Some(c[1].trim().to_string()),
        ),
        None => (t.trim().to_string(), None),
    }
}

pub fn parse(text: &str) -> Value {
    let lines: Vec<&str> = text.lines().collect();
    // A NOT_CAPTURED.md: only the bot's own description counts, not the export's preamble.
    let mut start = 0;
    if let Some(i) = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case("## The bot's description"))
    {
        start = i + 1;
        if lines[start..]
            .iter()
            .find(|l| !l.trim().is_empty())
            .is_some_and(|l| l.trim_start().starts_with("(not supplied)"))
        {
            start = lines.len();
        }
    }
    let (
        mut instructions,
        mut schedules,
        mut connectors,
        mut credentials,
        mut unparsed,
        mut warnings,
        mut manual,
    ) = (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    let mut sec = Sec::Pre;
    let mut sec_name = String::new();
    let mut para: Vec<&str> = vec![];
    let mut para_line = 0;
    let mut ctx_other = false;
    let unp = |u: &mut Vec<Value>, w: &mut Vec<String>, sec: &str, n: usize, t: &str| {
        let t = match secrets::guard_str(t) {
            Some(r) => {
                w.push(format!(
                    "unparsed line {n}: looks like it holds a secret ({r}), text not copied"
                ));
                not_copied(r)
            }
            None => t.trim().to_string(),
        };
        u.push(json!({"section": sec, "line": n, "text": t}));
    };
    let flush = |para: &mut Vec<&str>,
                 instructions: &mut Vec<Value>,
                 warnings: &mut Vec<String>,
                 n: usize| {
        if para.is_empty() {
            return;
        }
        let p = para.join("\n");
        match secrets::guard_str(&p) {
            Some(r) => {
                warnings.push(format!(
                    "instructions paragraph at line {n}: looks like it holds a secret ({r}), text not copied"
                ));
                instructions.push(json!(not_copied(r)));
            }
            None => instructions.push(json!(p)),
        }
        para.clear();
    };
    for (i, raw) in lines.iter().enumerate().skip(start) {
        let n = i + 1;
        let t = raw.trim();
        static H: OnceLock<Regex> = OnceLock::new();
        if let Some(c) = re(&H, r"^#{1,4}\s+(.+?)\s*#*$").captures(t) {
            flush(&mut para, &mut instructions, &mut warnings, para_line);
            ctx_other = false;
            sec_name = c[1].to_string();
            sec = section_of(&sec_name);
            continue;
        }
        if sec == Sec::Instructions {
            if t.is_empty() {
                flush(&mut para, &mut instructions, &mut warnings, para_line);
            } else {
                if para.is_empty() {
                    para_line = n;
                }
                para.push(raw.trim_end());
            }
            continue;
        }
        if t.is_empty() {
            ctx_other = false;
            continue;
        }
        if sec == Sec::Schedules && context_lead_in(t) {
            ctx_other = true;
            continue;
        }
        if sec != Sec::Pre && sec != Sec::Other && none_line(t) {
            continue;
        }
        // rule (A), keyword routing, for schedule and connector lines; rule (B) for anything else (credentials keep names)
        let hit = match sec {
            Sec::Schedules | Sec::Connectors => secrets::scan(t),
            Sec::Credentials => None,
            _ => secrets::guard_str(t),
        };
        if let Some(r) = hit {
            // A schedule or connector that looks like it holds a secret is not imported: the checklist gets a display
            // name, the line and the reason, never its text.
            let kind = match sec {
                Sec::Schedules => "schedule",
                Sec::Connectors => "connector",
                _ => {
                    unp(&mut unparsed, &mut warnings, &sec_name, n, t);
                    continue;
                }
            };
            let name = secrets::display(&split_item(t).0.unwrap_or_else(|| t.to_string()));
            warnings.push(format!(
                "{kind} at line {n} looks like it holds a secret ({r}): not imported, on the manual checklist"
            ));
            manual.push(json!({"kind": kind, "name": name, "line": n, "reason": r.to_string()}));
            continue;
        }
        match sec {
            Sec::Schedules => {
                let (name, rest) = split_item(t);
                let Some(name) = name else {
                    unp(&mut unparsed, &mut warnings, &sec_name, n, t);
                    continue;
                };
                let side = if ctx_other || other_side(t) {
                    "other"
                } else {
                    "grok"
                };
                let (when, prompt) = prompt_of(&rest);
                let text = schedule_text(&when);
                let cr = crate::adapters::hermes::schedule::normalize(&text).ok();
                let mut flags = vec![];
                if cr.is_none() && side == "grok" {
                    flags.push("not a Hermes schedule");
                    warnings.push(format!(
                        "schedule {name:?}: '{text}' is not a schedule Hermes accepts: not imported, on the manual checklist"
                    ));
                }
                let tz = match timezone(&when) {
                    Some(Ok(z)) => Some(z),
                    Some(Err(())) => {
                        flags.push("ambiguous timezone");
                        warnings.push(format!("schedule {name:?} has an ambiguous timezone"));
                        None
                    }
                    None => None,
                };
                if side == "grok" && tz.is_none() && flags.is_empty() {
                    flags.push("no timezone");
                    warnings.push(format!(
                        "schedule {name:?} has no timezone: it runs in the Hermes profile's timezone"
                    ));
                }
                schedules.push(
                    json!({"name": name, "when": when, "schedule": text, "cron": cr, "timezone": tz, "prompt": prompt,
                    "paused": t.to_ascii_lowercase().contains("(paused)"), "side": side, "flags": flags}),
                );
            }
            Sec::Connectors => {
                static U: OnceLock<Regex> = OnceLock::new();
                static T: OnceLock<Regex> = OnceLock::new();
                let (name, rest) = split_item(t);
                let Some(name) = name.map(|x| x.replace("**", "").replace('`', "")) else {
                    unp(&mut unparsed, &mut warnings, &sec_name, n, t);
                    continue;
                };
                // Prose ("Available but seldom needed for Demo: Mail, ...") is not a connector name.
                if name.split_whitespace().count() > 3 {
                    unp(&mut unparsed, &mut warnings, &sec_name, n, t);
                    continue;
                }
                let url = re(&U, r#"https?://[^\s)>"'`]+"#)
                    .find(&rest)
                    .map(|m| m.as_str().trim_end_matches(['.', ',']).to_string());
                let l = t.to_ascii_lowercase();
                let kind = if l.contains("mcp") {
                    "mcp"
                } else if url.is_some() {
                    "url"
                } else if l.contains("built-in") || l.contains("builtin") || l.contains("native") {
                    "builtin"
                } else {
                    "unknown"
                };
                let tools: Option<Vec<String>> = re(&T, r"(?i)\btools?\s*:\s*([^)\n]+)")
                    .captures(&rest)
                    .map(|c| {
                        c[1].split(',')
                            .map(|s| s.trim().trim_matches('`').to_string())
                            .filter(|s| tool_name(s))
                            .collect()
                    });
                connectors.push(json!({"name": name, "kind": kind, "url": url, "tools": tools}));
            }
            Sec::Credentials => {
                static MARK: OnceLock<Regex> = OnceLock::new();
                static NAME: OnceLock<Regex> = OnceLock::new();
                let body = re(&MARK, r"^\s*(?:[-*+]|\d+[.)])\s+").replace(t, "");
                // Only an identifier-shaped name: **bold**, `backticked` or UPPER_SNAKE. Prose is never guessed at.
                let Some(c) = re(
                    &NAME,
                    r"^(?:\*\*([A-Za-z][A-Za-z0-9_.\-]*)\*\*|`([A-Za-z][A-Za-z0-9_.\-]*)`|([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*))(?:[^A-Za-z0-9_]|$)",
                )
                .captures(&body) else {
                    unparsed.push(json!({"section": sec_name, "line": n, "text": NO_NAME}));
                    continue;
                };
                let m = c.get(1).or(c.get(2)).or(c.get(3)).unwrap();
                // A bare caps word without `_` ("CRM Postgres password") is prose unless a separator follows it.
                static SEP: OnceLock<Regex> = OnceLock::new();
                let bare_prose = c.get(3).is_some()
                    && !m.as_str().contains('_')
                    && !re(
                        &SEP,
                        r"^(?:\s*$|\s*[:=(]|\s+[-—–]\s|\s+(?:in|at|from|on)\s)",
                    )
                    .is_match(&body[m.end()..]);
                if c.get(3).is_some() && (m.as_str().len() < 3 || bare_prose) {
                    unparsed.push(json!({"section": sec_name, "line": n, "text": NO_NAME}));
                    continue;
                }
                let name = m.as_str().to_string();
                // The name itself is a value (`ghp_...`, AKIA...): nothing of the line is kept.
                if is_secret(&name) {
                    unparsed.push(json!({"section": sec_name, "line": n, "text": REDACTED}));
                    continue;
                }
                // Names only: a value-like name is routed; any other text on the line is never copied.
                if secrets::credential_name(&name).is_some() {
                    unparsed.push(json!({"section": sec_name, "line": n, "text": VALUE_NAME}));
                    continue;
                }
                let after = body[m.end()..].trim_start_matches(['*', '`']).trim();
                if !after.is_empty() {
                    warnings.push(format!(
                        "credential line had extra text, not copied (name {name})"
                    ));
                }
                credentials.push(json!({"name": name}));
            }
            _ => unp(&mut unparsed, &mut warnings, &sec_name, n, t),
        }
    }
    flush(&mut para, &mut instructions, &mut warnings, para_line);
    let by_name = |v: &mut Vec<Value>| {
        v.sort_by(|a, b| {
            a["name"]
                .as_str()
                .cmp(&b["name"].as_str())
                .then(a.to_string().cmp(&b.to_string()))
        })
    };
    by_name(&mut schedules);
    by_name(&mut connectors);
    by_name(&mut credentials);
    credentials.dedup_by(|a, b| a["name"] == b["name"]);
    json!({"schema": SCHEMA, "instructions": instructions, "schedules": schedules, "connectors": connectors,
           "credentials": credentials, "manual": manual, "unparsed": unparsed, "warnings": warnings})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::json_sorted;

    const STRUCTURED: &str = include_str!("../../../tests/fixtures/describe/structured.md");
    const MESSY: &str = include_str!("../../../tests/fixtures/describe/messy.md");
    const NONE: &str = include_str!("../../../tests/fixtures/describe/none.md");
    const SECRET: &str = include_str!("../../../tests/fixtures/describe/secret.md");

    fn crons(v: &Value) -> Vec<(String, Value, Value)> {
        v["schedules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_string(),
                    s["cron"].clone(),
                    s["timezone"].clone(),
                )
            })
            .collect()
    }

    #[test]
    fn structured_description() {
        let v = parse(STRUCTURED);
        assert_eq!(v["schema"], 1);
        assert_eq!(v["instructions"].as_array().unwrap().len(), 2);
        let c = crons(&v);
        assert!(
            c.contains(&(
                "Sample report".into(),
                json!("10 6 * * *"),
                json!("America/Chicago")
            )),
            "{c:?}"
        );
        assert!(
            c.contains(&("Ping check".into(), json!("every 15m"), json!("UTC"))),
            "{c:?}"
        );
        assert!(
            c.contains(&("Totals".into(), Value::Null, json!("Europe/London"))), // "hourly at :20" is not Hermes grammar
            "{c:?}"
        );
        assert!(
            c.contains(&(
                "Checkin".into(),
                json!("0 7 * * 1-5"),
                json!("America/Chicago")
            )),
            "{c:?}"
        );
        let backup = v["schedules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "Nightly export")
            .unwrap();
        assert_eq!(backup["side"], "other");
        let brief = v["schedules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "Sample report")
            .unwrap();
        assert_eq!(brief["prompt"], "Summarise the sample log");
        let conns = v["connectors"].as_array().unwrap();
        let demo = conns.iter().find(|c| c["name"] == "knowledge").unwrap();
        assert_eq!(demo["kind"], "mcp");
        assert_eq!(demo["url"], "https://demo.example.test/mcp");
        assert_eq!(demo["tools"], json!(["search", "fetch"]));
        assert_eq!(
            conns.iter().find(|c| c["name"] == "Web search").unwrap()["kind"],
            "builtin"
        );
        let creds: Vec<&str> = v["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(creds, vec!["EXAMPLE_API_KEY", "FEED_TOKEN"]);
        assert_eq!(v["unparsed"], json!([]));
    }

    #[test]
    fn messy_schedules_are_flagged_not_guessed_and_missing_timezone_is_flagged() {
        let v = parse(MESSY);
        let c = crons(&v);
        // "hourly", "whenever it feels right", "on every full lunar phase": none is Hermes grammar
        for n in ["Ping", "Mood check", "Tide report"] {
            assert!(c.contains(&(n.into(), Value::Null, Value::Null)), "{c:?}");
        }
        let ping = v["schedules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "Ping")
            .unwrap();
        assert_eq!(ping["flags"], json!(["not a Hermes schedule"]));
        assert_eq!(ping["schedule"], "hourly");
        let un = v["unparsed"].as_array().unwrap();
        let texts: Vec<&str> = un.iter().map(|u| u["text"].as_str().unwrap()).collect();
        assert!(
            texts.iter().any(|t| t.contains("random preamble")),
            "{texts:?}"
        );
        assert!(un
            .iter()
            .all(|u| u["line"].as_u64().unwrap() > 0 && u["section"].is_string()));
        let v = parse("## Schedules\n- **Ping**: every hour — prompt: \"ping\"\n");
        let ping = &v["schedules"][0];
        assert_eq!(ping["cron"], "every 60m");
        assert_eq!(ping["flags"], json!(["no timezone"]));
        assert!(
            v["warnings"]
                .to_string()
                .contains("runs in the Hermes profile's timezone"),
            "{v}"
        );
    }

    #[test]
    fn none_means_zero_schedules() {
        let v = parse(NONE);
        assert_eq!(v["schedules"], json!([]));
        assert_eq!(v["connectors"], json!([]));
        assert_eq!(v["credentials"], json!([]));
        assert_eq!(v["unparsed"], json!([]));
        assert_eq!(parse("")["schedules"], json!([]));
    }

    #[test]
    fn secret_values_never_reach_the_output() {
        let v = parse(SECRET);
        let out = json_sorted(&v);
        for bad in [
            "sk-ABCDEFGHIJKLMNOPQRSTUVWX123",
            "hunter2hunter2",
            "ghp_abcdefghijklmnop1234",
            "xai-ZZZZZZZZZZZZZZZZZZZZ",
        ] {
            assert!(!out.contains(bad), "leaked {bad}: {out}");
        }
        let creds: Vec<&str> = v["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(
            creds.contains(&"OPENAI_KEY")
                && creds.contains(&"DB_PASSWORD")
                && creds.contains(&"GITHUB_TOKEN"),
            "{creds:?}"
        );
    }

    #[test]
    fn not_captured_wrapper_is_skipped_and_output_is_deterministic() {
        let wrapped = format!("# Not captured by the export\n\n- **Instructions** (live system prompt)\n\n## The bot's description\n\n{STRUCTURED}");
        let a = json_sorted(&parse(&wrapped));
        assert_eq!(a, json_sorted(&parse(STRUCTURED)));
        assert_eq!(a, json_sorted(&parse(&wrapped)));
        let blank = parse("# Not captured\n\n## The bot's description\n\n(not supplied) Ask the bot:\n\n> Describe ...\n");
        assert_eq!(blank["unparsed"], json!([]));
        assert_eq!(blank["instructions"], json!([]));
    }

    #[test]
    fn credential_where_is_location_only() {
        let v = parse("## Credentials\n- SMTP pass is Tr0ub4dor&3\n- GITHUB_TOKEN in ~/.env\n- OPENAI_KEY — in the owner's .env\n- SLACK_TOKEN from the credential store\n");
        let s = v.to_string();
        assert!(!s.contains("Tr0ub4dor"), "{s}");
        let c = v["credentials"].as_array().unwrap();
        let w = |n: &str| c.iter().find(|x| x["name"] == n).unwrap()["where"].clone();
        assert!(c.iter().all(|x| x["name"] != "SMTP"), "prose, not a name");
        for n in ["GITHUB_TOKEN", "OPENAI_KEY", "SLACK_TOKEN"] {
            assert_eq!(w(n), Value::Null, "names only");
        }
        assert_eq!(v["unparsed"][0]["line"], 2, "{s}");
    }

    #[test]
    fn credential_prose_never_guesses_a_short_caps_word_and_keeps_no_text() {
        let v = parse("## Credentials\n- CRM Postgres password is hunter2hunter\n- CRM_PG_PASSWORD: in the vault\n- SMTP: in ~/.env\n");
        let s = v.to_string();
        assert!(!s.contains("hunter2hunter"), "{s}");
        let names: Vec<&str> = v["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["CRM_PG_PASSWORD", "SMTP"]);
        assert_eq!(v["unparsed"][0]["line"], 2);
    }

    #[test]
    fn credential_where_stops_at_the_location_phrase() {
        // Regression: a value after a comma must never reach `where`.
        let v = parse("## Credentials\n- STRIPE_KEY in vault, value is pa55w0rdXYZ\n- MAIL_PASS in ~/.env; it is hunter2x\n- SMS_KEY in the vault - the password is abc123zz\n- DB_URL is postgres-in-prod\n");
        let s = v.to_string();
        for leak in ["pa55w0rdXYZ", "hunter2x", "abc123zz", "postgres-in-prod"] {
            assert!(!s.contains(leak), "{leak} leaked: {s}");
        }
        let c = v["credentials"].as_array().unwrap();
        let w = |n: &str| c.iter().find(|x| x["name"] == n).unwrap()["where"].clone();
        // the whole line looks like it holds a secret, so not even the location is kept
        assert_eq!(w("STRIPE_KEY"), Value::Null);
        assert_eq!(w("MAIL_PASS"), Value::Null);
        assert_eq!(w("SMS_KEY"), Value::Null);
        assert_eq!(w("DB_URL"), Value::Null);
        assert!(s.contains("(name STRIPE_KEY)"), "{s}");
    }

    #[test]
    fn credential_without_an_identifier_name_is_unparsed() {
        // Regression: prose must not give a guessed name ("The").
        let v = parse("## Credentials\n- The GitHub token is in ~/.env\n- **Stripe**: in vault\n- `smtp-login` in ~/.env\n");
        let names: Vec<&str> = v["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Stripe", "smtp-login"]);
        let u = v["unparsed"].as_array().unwrap();
        assert_eq!(u.len(), 1, "{u:?}");
        assert_eq!(u[0]["line"], 2);
    }

    #[test]
    fn timezone_tokens_attached_to_time_only() {
        assert_eq!(timezone("Plan Z daily at 7am"), None);
        assert_eq!(timezone("daily 07:00 Z"), Some(Ok("UTC".into())));
        assert_eq!(
            timezone("daily at 7am PST"),
            Some(Ok("America/Los_Angeles".into()))
        );
        assert_eq!(timezone("GMT review daily 9am"), None);
        assert_eq!(timezone("daily 9am CST"), Some(Err(())));
        let v = parse("## Schedules\n- **Rpt**: daily at 9am CST\n");
        let s = &v["schedules"][0];
        assert_eq!(s["timezone"], Value::Null);
        assert!(s["flags"].to_string().contains("ambiguous timezone"), "{s}");
    }

    #[test]
    fn only_what_hermes_accepts_becomes_a_schedule() {
        let v = parse("## Schedules\n- **A**: weekdays 7am America/Chicago\n- **B**: 7am weekdays\n- **C**: `0 2 * * *` at UTC (paused)\n- **D**: every 15 min CDT\n- **E**: nightly 2am UTC\n- **F**: daily at 9am CST\n");
        let by = |n: &str| {
            v["schedules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(by("A")["cron"], "0 7 * * 1-5");
        assert_eq!(by("C")["cron"], "0 2 * * *");
        assert_eq!(by("C")["timezone"], "UTC");
        assert_eq!(by("D")["cron"], "every 15m");
        assert_eq!(by("D")["timezone"], "America/Chicago");
        // English Hermes does not accept is kept as text and flagged, never guessed into cron
        for (n, text) in [
            ("B", "7am weekdays"),
            ("E", "nightly 2am"),
            ("F", "daily at 9am CST"),
        ] {
            let s = by(n);
            assert_eq!(s["cron"], Value::Null, "{s}");
            assert_eq!(s["schedule"], text, "{s}");
            assert!(
                s["flags"].to_string().contains("not a Hermes schedule"),
                "{s}"
            );
        }
        assert!(v["unparsed"].as_array().unwrap().is_empty(), "{v}");
    }

    #[test]
    fn a_token_shaped_name_or_where_never_reaches_the_output() {
        // Regressions: a token as the name, a value after `:` or a token as the location.
        let v = parse("## Credentials\n- `ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789`\n- AKIAIOSFODNN7EXAMPL3\n- MY_KEY at ~/.env:XyZ9988776655\n- SLACK_TOKEN in xoxp-1234-5678-abcdefgh\n- OK_KEY in ~/.env\n");
        let s = v.to_string();
        for leak in [
            "aBcDeFgHiJkLmNoPqRsTuVwXyZ",
            "IOSFODNN7EXAMPL3",
            "XyZ9988776655",
            "xoxp-1234",
        ] {
            assert!(!s.contains(leak), "{leak} leaked: {s}");
        }
        let c = v["credentials"].as_array().unwrap();
        let w = |n: &str| c.iter().find(|x| x["name"] == n).unwrap()["where"].clone();
        assert_eq!(c.len(), 3, "{s}");
        assert_eq!(w("MY_KEY"), Value::Null);
        assert_eq!(w("SLACK_TOKEN"), Value::Null);
        assert_eq!(w("OK_KEY"), Value::Null);
        assert_eq!(v["unparsed"].as_array().unwrap().len(), 2, "{s}");
    }

    #[test]
    fn credentials_are_names_only_and_a_value_like_name_is_routed() {
        // Regressions (synthetic values): no `where` key, no location text, no name embedding a value.
        let v = parse("## Credentials\n- **VAULT_PW** — kept in zqhunter vault\n- `DB_PASS`: the zqtr0ub4dor host\n- **PW_zqhunterpass**\n- `openai_api_key` in ~/.env\n");
        let s = v.to_string();
        for leak in ["zqhunter", "zqtr0ub4dor", "vault", "~/.env"] {
            assert!(!s.contains(leak), "{leak} leaked: {s}");
        }
        let c = v["credentials"].as_array().unwrap();
        assert!(
            c.iter().all(|x| x.as_object().unwrap().keys().eq(["name"])),
            "{s}"
        );
        let names: Vec<&str> = c.iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["DB_PASS", "VAULT_PW", "openai_api_key"]);
        assert!(s.contains("name may contain a value"), "{s}");
    }

    #[test]
    fn connector_names_lose_markup_and_prose_is_not_a_name() {
        let v = parse("## Connectors\n- **notes** MCP: https://n.example.test/mcp\n- Available but seldom needed for Demo: Mail, Drive\n");
        let names: Vec<&str> = v["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["notes MCP"]);
        assert_eq!(v["unparsed"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn section_context_makes_the_following_lines_other_side() {
        let v = parse(include_str!("../../../tests/fixtures/describe/context.md"));
        let s = v["schedules"].as_array().unwrap();
        assert_eq!(s.len(), 3, "{v}");
        assert!(s.iter().all(|x| x["side"] == "other"), "{v}");
        // A blank line or a heading ends the context.
        let v = parse("## Schedules\nNone on the Grok side. Home side, for context:\n- a: daily 7am UTC\n\n- **b**: daily 8am UTC — prompt: \"go\"\n");
        let side = |n: &str| {
            v["schedules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["name"] == n)
                .unwrap()["side"]
                .clone()
        };
        assert_eq!(side("a"), "other");
        assert_eq!(side("b"), "grok");
    }

    #[test]
    fn a_suspicious_schedule_or_connector_goes_to_manual_with_no_text() {
        let v = parse("## Connectors\n- **notes**: MCP server at https://user:Pa55wordQQ99@notes.example.test/mcp\n- **crm**: MCP at https://crm.example.test/mcp?api_token=abc123abc123\n- **frag**: MCP at https://m.example.test/mcp#access_token=QVXf\n- **plain**: MCP at https://demo.example.test/mcp?x=1\n## Schedules\n- **Poll**: hourly — prompt: \"fetch https://u:Pw99xx@h.test/x\"\n- **Login**: daily 9am UTC — prompt: \"Log in with password QVXlogin and check\"\n- **Totals**: daily 9am UTC — prompt: \"summarise the key points\"\n"); // trufflehog:ignore (fake)
        let s = v.to_string();
        for leak in ["Pa55wordQQ99", "abc123abc123", "Pw99xx", "QVXlogin", "QVXf"] {
            assert!(!s.contains(leak), "{leak} leaked: {s}");
        }
        let m: Vec<(String, String)> = v["manual"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x["kind"].as_str().unwrap().into(),
                    x["name"].as_str().unwrap().into(),
                )
            })
            .collect();
        for (k, n) in [
            ("connector", "notes"),
            ("connector", "crm"),
            ("connector", "frag"),
            ("schedule", "Poll"),
            ("schedule", "Login"),
        ] {
            assert!(m.contains(&(k.into(), n.into())), "{k} {n}: {m:?}");
        }
        assert!(v["manual"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["reason"].is_string() && x["line"].is_u64()));
        let c = v["connectors"].as_array().unwrap();
        assert_eq!(c.len(), 1, "{s}");
        // clean text is copied byte-identical (the url too)
        assert_eq!(c[0]["url"], "https://demo.example.test/mcp?x=1");
        let sch = v["schedules"].as_array().unwrap();
        assert_eq!(sch.len(), 1, "{s}");
        assert_eq!(sch[0]["prompt"], "summarise the key points");
    }

    #[test]
    fn suspicious_free_text_becomes_a_marker() {
        let v = parse("## Instructions\nUse key: QVXinstrkeyvalue for the API.\n\nBe nice.\n## Notes\npassword: QVXnotes\nplain note\n");
        let s = v.to_string();
        assert!(!s.contains("QVXinstr") && !s.contains("QVXnotes"), "{s}");
        assert_eq!(v["instructions"][1], "Be nice.");
        assert!(v["instructions"][0]
            .as_str()
            .unwrap()
            .starts_with("[text not copied: "));
        assert_eq!(v["unparsed"][1]["text"], "plain note");
    }

    #[test]
    fn paused_comes_from_the_line_not_a_raw_field() {
        let v = parse("## Schedules\n- **A**: daily 7am UTC (paused) — prompt: \"go\"\n");
        assert_eq!(v["schedules"][0]["paused"], true);
        assert!(v["schedules"][0].get("raw").is_none());
    }
}

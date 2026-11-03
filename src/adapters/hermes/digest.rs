//! The Hermes side of the activity digest (docs/design/MIRROR.md section 5.2): build `botport/digest-hermes.md` from a
//! profile's own `state.db` (owner sessions only, read with the `sqlite3` command, read-only, as an argv), its
//! `cron/jobs.json`, the skill folders under `skills/` (counted, never opened) and the times and conflict count of
//! `botport.mirror.json`. Summary level only: routine names, schedules, time zones and last-run status, counts, times
//! and session titles (with `--titles`), only the last message's role and whether it ends in a question mark, never its
//! text; never memories or user facts (`memories/` is never read); routine names as written. A script, never a model
//! call; no network; no operation lock (it writes only its own digest file, atomically).
//!
//! Hermes' formats, as published (hermes-agent at c314285): the `state.db` sessions and messages tables
//! (https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/website/docs/developer-guide/session-storage.md#L182-L253), session sources
//! (https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/website/docs/user-guide/sessions.md#L113-L118), cron job records
//! (https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/cron/jobs.py#L1896-L1940), skill folders
//! (https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/website/docs/user-guide/features/skills.md#L336-L356).
use super::sync;
use crate::core::digest::{self, Digest, FACTS, OPEN, RECENT, ROUTINES};
use crate::core::util::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SIDE: &str = "hermes";
/// Where the digest goes, under the profile.
pub const DIR: &str = "botport";
pub const OWN_FILE: &str = "digest-hermes.md";
/// The peer's digest, as `mirror sync` delivers it.
pub const PEER_FILE: &str = "digest-grok.md";
/// Session sources (and hook `platform` values) that are the owner at the machine (`cron` never is). `digest write`
/// reads only these sessions or those of listed owner user ids; `digest inject` adds context only when the hook's
/// `platform` is one of these or its `sender_id` is a listed owner id, and prints nothing otherwise (fails closed).
pub const LOCAL_SOURCES: [&str; 4] = ["cli", "tui", "desktop", "local"];

pub struct Opts {
    /// pair name for the header (default: the profile dir's name)
    pub pair: String,
    pub window_days: u32,
    /// user ids that are the owner on a messaging platform
    pub owner_users: Vec<String>,
    /// Unix seconds
    pub now: i64,
    /// Leave session titles out (they are usually generated from the conversation): each session is "session".
    pub no_titles: bool,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Wrote(PathBuf),
    Unchanged(PathBuf),
    /// state.db is locked: nothing written; the next session end writes
    Busy,
}

/// Why the session sections are empty, if they are (said on stderr; the command still succeeds).
#[derive(Debug, PartialEq)]
enum Sessions {
    Read(Vec<Session>),
    Busy,
    Unavailable(String),
}

/// The label of a session without a title (its messages are never read for one).
const UNTITLED: &str = "untitled session";
/// The label of every session unless the owner asks for titles (`--titles`).
const NO_TITLE: &str = "session";

#[derive(Debug, PartialEq, Default)]
struct Session {
    started: f64,
    ended: Option<f64>,
    messages: i64,
    label: String,
    last_role: String,
    last_char: String,
}

/// Build and write the digest unless state.db is busy. An unchanged digest is not rewritten.
pub fn write(profile: &Path, o: &Opts) -> Res<Outcome> {
    let Some(d) = build(profile, o)? else {
        return Ok(Outcome::Busy);
    };
    let root = profile.join(DIR);
    let p = root.join(OWN_FILE);
    Ok(
        match digest::write_if_changed(&root, OWN_FILE, &d.render())? {
            true => Outcome::Wrote(p),
            false => Outcome::Unchanged(p),
        },
    )
}

/// The digest for `profile`, or `None` when state.db is busy.
pub fn build(profile: &Path, o: &Opts) -> Res<Option<Digest>> {
    if !profile.is_dir() {
        return err(format!("profile not found: {}", profile.display()));
    }
    let mut d = Digest {
        side: SIDE.into(),
        pair: o.pair.clone(),
        written: iso_utc(o.now),
        window_days: o.window_days,
        sections: Default::default(),
    };
    match sessions(profile, o) {
        Sessions::Busy => return Ok(None),
        Sessions::Unavailable(why) => {
            eprintln!("botport: note: sessions left out of the digest: {why}")
        }
        Sessions::Read(ss) => {
            d.sections[RECENT] = ss.iter().map(recent_line).collect();
            d.sections[OPEN] = ss.iter().filter_map(open_line).collect();
        }
    }
    let jobs = sync::live_jobs(profile).unwrap_or_else(|e| {
        eprintln!("botport: note: routines left out of the digest: {e}");
        vec![]
    });
    d.sections[ROUTINES] = routine_lines(&jobs);
    d.sections[FACTS] = fact_lines(profile, &jobs);
    Ok(Some(d))
}

/// `MM-DD HH:MM` (UTC) of Unix seconds.
fn when(secs: f64) -> String {
    let (_, m, d, hh, mm, _) = utc_civil(secs as i64);
    format!("{m:02}-{d:02} {hh:02}:{mm:02}")
}

fn recent_line(s: &Session) -> String {
    let n = s.messages;
    let unit = if n == 1 { "message" } else { "messages" };
    format!("{} {} ({n} {unit})", when(s.started), s.label)
}

/// Open: the last message is a question to the owner, or from the owner, or the session has not ended:
/// `MM-DD HH:MM <title> (<reason>)`.
fn open_line(s: &Session) -> Option<String> {
    let why = match (s.last_role.as_str(), s.last_char.as_str()) {
        ("assistant", "?") => "last message was a question to the owner",
        ("user", _) => "last message was from the owner",
        _ if s.ended.is_none() => "not ended",
        _ => return None,
    };
    // the label gives way, so the reason is never cut off; the reason goes in parentheses, not after `: `, so a title
    // ending in a word like "session" or "token" does not read as `key: value` to the credential check
    let room = digest::LINE_MAX - 2 - "MM-DD HH:MM ".len() - " ()".len() - why.len();
    Some(format!(
        "{} {} ({why})",
        when(s.started),
        cut(&s.label, room)
    ))
}

/// A string literal for SQL (`'` doubled). Only for values checked by [`owner_id_ok`].
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// An owner user id botport puts into a query: 1-128 characters, no control characters.
pub fn owner_id_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().count() <= 128 && !s.chars().any(char::is_control)
}

/// The query: owner sessions active in the window, newest first: times, message count, title (with `--titles`), and
/// only the last message's role and whether it ends in a question mark, never its text: no message text, tool arguments or results leave the database.
fn query(o: &Opts) -> String {
    let since = o.now - i64::from(o.window_days) * 86400;
    let local: Vec<String> = LOCAL_SOURCES.iter().map(|s| sql_str(s)).collect();
    // a local surface is the owner only without a user id: a desktop session opened through an authenticated
    // `hermes serve` stores that person's login as user_id (https://github.com/NousResearch/hermes-agent/blob/c314285f077e91937c995d419b9fa40af9eb4981/website/docs/developer-guide/session-storage.md#L221-L226)
    let mut owner = format!(
        "(lower(coalesce(s.source, '')) IN ({}) AND coalesce(s.user_id, '') = '')",
        local.join(", ")
    );
    let users: Vec<String> = o
        .owner_users
        .iter()
        .filter(|u| owner_id_ok(u))
        .map(|u| sql_str(u))
        .collect();
    if !users.is_empty() {
        owner = format!("({owner} OR s.user_id IN ({}))", users.join(", "));
    }
    // the rows a session still shows: active messages and compaction summaries (inactive rows are skipped)
    let shown =
        "m.session_id = s.id AND (coalesce(m.active, 1) = 1 OR coalesce(m.compacted, 0) = 1)";
    let last =
        format!("FROM messages m WHERE {shown} ORDER BY m.timestamp DESC, m.id DESC LIMIT 1");
    // a session's title is read only when the owner asked for titles (`--titles`)
    let title = if o.no_titles {
        "''"
    } else {
        "substr(coalesce(s.title, ''), 1, 300)"
    };
    format!(
        "SELECT s.started_at AS started, s.ended_at AS ended, \
         coalesce(s.message_count, (SELECT count(*) FROM messages m WHERE {shown})) AS messages, \
         {title} AS title, \
         (SELECT m.role {last}) AS last_role, \
         (SELECT CASE WHEN substr(rtrim(coalesce(m.content, ''), ' ' || char(9, 10, 13)), -1) = '?' THEN '?' ELSE '' END {last}) AS last_char \
         FROM sessions s \
         WHERE lower(coalesce(s.source, '')) <> 'cron' AND {owner} \
         AND max(coalesce(s.started_at, 0), coalesce(s.ended_at, 0)) >= {since} \
         ORDER BY s.started_at DESC, s.id DESC LIMIT 100"
    )
}

/// Owner sessions from `<profile>/state.db`, read with `sqlite3 -readonly -json` and a 200 ms busy timeout.
fn sessions(profile: &Path, o: &Opts) -> Sessions {
    let db = profile.join("state.db");
    if !std::fs::symlink_metadata(&db).is_ok_and(|m| m.is_file()) {
        return Sessions::Unavailable("no state.db".into());
    }
    // absolute, so the argument can never read as an option or a `file:` URI
    let db = match std::path::absolute(&db) {
        Ok(p) => p,
        Err(e) => return Sessions::Unavailable(format!("state.db: {e}")),
    };
    // by absolute path from a checked PATH (sqlite3 3.33 or later, for -json)
    let sqlite3 = match crate::core::util::tool("sqlite3") {
        Ok(p) => p,
        Err(e) if e.contains("not found") => {
            return Sessions::Unavailable("the sqlite3 command is not installed".into())
        }
        Err(e) => return Sessions::Unavailable(e),
    };
    let out = Command::new(sqlite3)
        .args(["-readonly", "-json", "-cmd", ".timeout 200"])
        .arg(&db)
        .arg(query(o))
        .stdin(Stdio::null())
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => return Sessions::Unavailable(format!("sqlite3: {e}")),
    };
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if stderr.contains("database is locked") || stderr.contains("database is busy") {
        return Sessions::Busy;
    }
    if !out.status.success() {
        // the first line only: an error names the schema, never row content
        let first = stderr
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect::<String>();
        return Sessions::Unavailable(format!("sqlite3 failed ({})", first.trim()));
    }
    match parse_rows(&out.stdout).map(|mut ss| {
        if o.no_titles {
            for s in &mut ss {
                s.label = NO_TITLE.into();
            }
        }
        ss
    }) {
        Ok(ss) => Sessions::Read(ss),
        Err(e) => Sessions::Unavailable(e),
    }
}

/// `sqlite3 -json` output (nothing at all for no rows) -> sessions, rows in query order.
fn parse_rows(stdout: &[u8]) -> Res<Vec<Session>> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(vec![]);
    }
    let rows: Vec<Value> =
        serde_json::from_slice(stdout).map_err(|e| format!("sqlite3 output is not JSON: {e}"))?;
    let num = |v: &Value| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    };
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    Ok(rows
        .iter()
        .filter_map(|r| {
            let first_line = |v: &Value| {
                v.as_str()
                    .unwrap_or("")
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .to_string()
            };
            let label = first_line(&r["title"]);
            // cleaned here too, so a heading or link at the start of a title is not left mid-line (a credential is
            // kept raw: the render drops the whole line)
            let label = digest::clean(&label).unwrap_or(label);
            Some(Session {
                started: num(&r["started"])?,
                ended: num(&r["ended"]),
                messages: num(&r["messages"]).unwrap_or(0.0) as i64,
                label: cut(if label.is_empty() { UNTITLED } else { &label }, 120),
                last_role: text(&r["last_role"]),
                last_char: text(&r["last_char"]),
            })
        })
        .collect())
}

/// At most `n` characters, `…` marking a cut.
fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars()
        .take(n - 1)
        .collect::<String>()
        .trim_end()
        .to_string()
        + "…"
}

/// cron/jobs.json -> `name: schedule (tz)[, paused][, last ran MM-DD HH:MM status]`, by name.
fn routine_lines(jobs: &[Value]) -> Vec<String> {
    let mut out: Vec<(String, String)> = jobs
        .iter()
        .filter_map(|j| {
            let name = j["name"].as_str().filter(|n| !n.trim().is_empty())?.trim();
            let mut sched = sync::cron_expr(j);
            if sched.is_empty() {
                sched = match j["schedule"]["minutes"].as_u64() {
                    Some(m) => format!("every {m}m"),
                    None => "no schedule".into(),
                };
            }
            let tz = j["timezone"]
                .as_str()
                .filter(|t| !t.is_empty())
                .unwrap_or("local");
            let mut l = format!("{}: {} ({})", cut(name, 80), cut(&sched, 40), cut(tz, 20));
            if !job_enabled(j) {
                l += ", paused";
            }
            let last = &j["last_run_at"];
            let ran = last.as_str().and_then(parse_iso);
            if let Some(t) = ran {
                let status = j["last_status"]
                    .as_str()
                    .unwrap_or(if j["last_error"].is_string() {
                        "error"
                    } else {
                        ""
                    });
                l += &format!(", last ran {}", when(t as f64));
                if !status.is_empty() {
                    l += &format!(" {}", cut(status, 20));
                }
            }
            Some((name.to_string(), l))
        })
        .collect();
    out.sort();
    out.into_iter().map(|(_, l)| l).collect()
}

/// Setup facts, never memories: `N skills, M routines (K enabled)`, and with a mirror state `last mirror change MM-DD
/// HH:MM, N open conflicts`.
fn fact_lines(profile: &Path, jobs: &[Value]) -> Vec<String> {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let enabled = jobs.iter().filter(|j| job_enabled(j)).count();
    let mut out = vec![format!(
        "{}, {} ({enabled} enabled)",
        plural(skill_count(&profile.join("skills")), "skill", "skills"),
        plural(jobs.len(), "routine", "routines")
    )];
    if let Some(l) = mirror_line(&profile.join("botport.mirror.json")) {
        out.push(l);
    }
    out
}

fn job_enabled(j: &Value) -> bool {
    super::mirror::job_runnable(j)
}

fn is_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Skill folders, counted from directory entries only (no file is opened): `skills/<name>/` holding a SKILL.md is one
/// skill, any other `skills/<category>/` counts its sub-folders. Hidden entries and symlinks are skipped.
fn skill_count(root: &Path) -> usize {
    let dirs = |d: &Path| -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(d) else {
            return vec![];
        };
        rd.flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path())
            .filter(|p| is_dir(p))
            .collect()
    };
    dirs(root)
        .iter()
        .map(|d| {
            if d.join("SKILL.md").is_file() {
                1
            } else {
                dirs(d).len()
            }
        })
        .sum()
}

/// From the mirror state, only `items.*.at` (the newest) and the length of `conflicts`. `None` without a readable one.
fn mirror_line(p: &Path) -> Option<String> {
    let md = std::fs::symlink_metadata(p).ok()?;
    if !md.is_file() || md.len() > 16 << 20 {
        return None;
    }
    let v: Value = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
    let newest = v["items"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(|i| i["at"].as_str().and_then(parse_iso))
        .max();
    let conflicts = v["conflicts"].as_array().map_or(0, Vec::len);
    let n = format!(
        "{conflicts} open conflict{}",
        if conflicts == 1 { "" } else { "s" }
    );
    Some(match newest {
        Some(t) => format!("last mirror change {}, {n}", when(t as f64)),
        None => format!("no mirror changes yet, {n}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Opts {
        Opts {
            pair: "demo-bot".into(),
            window_days: 7,
            owner_users: vec!["owner-1".into(), "o'quote".into(), "bad\nid".into()],
            now: 946_771_200,
            no_titles: false,
        }
    }

    #[test]
    fn the_query_selects_owner_sessions_only() {
        let q = query(&opts());
        assert!(q.contains("IN ('cli', 'tui', 'desktop', 'local')"));
        assert!(q.contains("s.user_id IN ('owner-1', 'o''quote')"), "{q}");
        assert!(!q.contains("bad"));
        assert!(q.contains("<> 'cron'"));
        // a title is read only with `--titles`
        assert!(q.contains("s.title"), "{q}");
        let untitled = query(&Opts {
            no_titles: true,
            ..opts()
        });
        assert!(!untitled.contains("s.title"), "{untitled}");
        // of the message text, only whether the last message ends in a question mark
        assert_eq!(q.matches("m.content").count(), 1);
        assert!(q.contains("CASE WHEN substr(rtrim(coalesce(m.content, ''), ' ' || char(9, 10, 13)), -1) = '?' THEN '?' ELSE '' END"));
        assert!(q.contains(&format!(">= {}", 946_771_200 - 7 * 86400)));
        let none = query(&Opts {
            owner_users: vec![],
            ..opts()
        });
        assert!(!none.contains("user_id IN"));
    }

    #[test]
    fn rows_become_recent_and_open_lines() {
        let rows = br#"[
          {"started": 946699200.5, "ended": 946700200, "messages": 12, "title": "Example session title",
           "last_role": "assistant", "last_char": "?"},
          {"started": 946639200, "ended": null, "messages": 1, "title": "\n  Example first line\nsecond",
           "last_role": "assistant", "last_char": "."},
          {"started": 946539200, "ended": 946539300, "messages": 3, "title": null,
           "last_role": "user", "last_char": "k"},
          {"started": 946439200, "ended": 946439300, "messages": 2, "title": "Done",
           "last_role": "assistant", "last_char": "."},
          {"started": null}
        ]"#;
        let ss = parse_rows(rows).unwrap();
        assert_eq!(ss.len(), 4);
        let recent: Vec<String> = ss.iter().map(recent_line).collect();
        assert_eq!(recent[0], "01-01 04:00 Example session title (12 messages)");
        assert_eq!(recent[1], "12-31 11:20 Example first line (1 message)");
        assert_eq!(recent[2], "12-30 07:33 untitled session (3 messages)");
        let open: Vec<String> = ss.iter().filter_map(open_line).collect();
        assert_eq!(
            open,
            [
                "01-01 04:00 Example session title (last message was a question to the owner)",
                "12-31 11:20 Example first line (not ended)",
                "12-30 07:33 untitled session (last message was from the owner)",
            ]
        );
        assert!(parse_rows(b"").unwrap().is_empty());
        assert!(parse_rows(b"not json").is_err());
    }

    #[test]
    fn facts_are_setup_counts_and_mirror_times_never_memories() {
        let d = std::env::temp_dir().join(format!("botport-hfacts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for s in [
            "skills/demo-cat/demo-skill",
            "skills/demo-cat/demo-skill-two",
            "skills/.hub/x",
        ] {
            std::fs::create_dir_all(d.join(s)).unwrap();
        }
        std::fs::create_dir_all(d.join("skills/demo-solo")).unwrap();
        std::fs::write(d.join("skills/demo-solo/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(d.join("memories")).unwrap();
        std::fs::write(
            d.join("memories/MEMORY.md"),
            "Example fact: never in the digest.",
        )
        .unwrap();
        let jobs: Vec<Value> =
            serde_json::from_str(r#"[{"name": "a"}, {"name": "b", "enabled": false}]"#).unwrap();
        assert_eq!(fact_lines(&d, &jobs), ["3 skills, 2 routines (1 enabled)"]);
        std::fs::write(
            d.join("botport.mirror.json"),
            r#"{"items": {"skill:demo-skill": {"at": "2000-01-01T09:00:00Z", "base_a": "Example content"},
                "routine:demo-routine": {"at": "1999-12-30T22:00:00Z"}},
               "conflicts": [{"item": "persona.instructions"}], "approvals": [{"item": "x"}]}"#,
        )
        .unwrap();
        assert_eq!(
            fact_lines(&d, &[]),
            [
                "3 skills, 0 routines (0 enabled)",
                "last mirror change 01-01 09:00, 1 open conflict"
            ]
        );
        std::fs::write(d.join("botport.mirror.json"), r#"{"items": {}}"#).unwrap();
        assert_eq!(
            fact_lines(&d, &[])[1],
            "no mirror changes yet, 0 open conflicts"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn routines_read_name_schedule_tz_and_last_run() {
        let jobs: Vec<Value> = serde_json::from_str(
            r#"[
              {"name": "demo-routine", "schedule": {"kind": "cron", "expr": "15 3 * * *"}, "timezone": "UTC",
               "last_run_at": "2000-01-01T03:15:00+00:00", "last_status": "ok"},
              {"name": "another-routine", "schedule": {"kind": "interval", "minutes": 30}, "enabled": false},
              {"name": "", "schedule": {"expr": "* * * * *"}}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            routine_lines(&jobs),
            [
                "another-routine: every 30m (local), paused",
                "demo-routine: 15 3 * * * (UTC), last ran 01-01 03:15 ok",
            ]
        );
    }
}

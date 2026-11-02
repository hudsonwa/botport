//! grok-bot-export v1: the JSON a Grok Bot writes about itself for upstream `hermes migrate grok-bot`
//! (agent-importers `grok_bot.py`). Read into the neutral model, or into a `Bundle` so plan/apply/sync take it.
use super::bundle::Bundle;
use crate::core::model::*;
use crate::core::util::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const FORMAT: &str = "grok-bot-export";
/// Upstream's skill folder for imported skills (`skills/grok-bot-imports/<slug>`).
pub const IMPORTS: &str = "grok-bot-imports";
const TRUNCATED: &str = "[truncated";
const SCHEDULE_TRIGGERS: [&str; 5] = ["schedule", "scheduled", "time", "cron", "interval"];

/// True when `p` is a file holding a grok-bot-export document (cheap check, as upstream does).
pub fn is_export(p: &Path) -> bool {
    p.is_file()
        && std::fs::read(p)
            .map(|b| String::from_utf8_lossy(&b).contains(FORMAT))
            .unwrap_or(false)
}

/// Upstream `_slug`: lowercase, runs of non-alphanumerics -> `-`, at most 64 chars.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch);
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s: String = s.trim_matches('-').chars().take(64).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() {
        "grok-bot".into()
    } else {
        s
    }
}

/// Short, stable id for an app with a free-text name: the part before the first `(`, `;` or ` - `, slugged,
/// at most 32 chars on a word boundary.
pub fn app_id(name: &str) -> String {
    let head = name.split(['(', ';']).next().unwrap_or(name);
    let head = head.split(" - ").next().unwrap_or(head);
    let mut out = String::new();
    for w in slug(head).split('-') {
        if !out.is_empty() && out.len() + 1 + w.len() > 32 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(w);
    }
    out.chars().take(32).collect()
}

/// Upstream `_text`: stripped; with no real newline, literal `\n`/`\t`/`\"` escapes become characters.
pub fn text(v: &Value) -> String {
    let t = v.as_str().unwrap_or("").trim().to_string();
    if !t.contains('\n') && t.contains("\\n") {
        return t
            .replace("\\r\\n", "\n")
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"");
    }
    t
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| match x {
                    Value::String(s) => s.trim().to_string(),
                    o => o.to_string(),
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn dicts(v: &Value) -> Vec<&Value> {
    v.as_array()
        .map(|a| a.iter().filter(|x| x.is_object()).collect())
        .unwrap_or_default()
}

fn secret_key(k: &str) -> bool {
    let k = k.to_lowercase();
    [
        "password",
        "secret",
        "token",
        "api_key",
        "apikey",
        "cookie",
        "private_key",
    ]
    .iter()
    .any(|s| k.contains(s))
}

fn secret_paths(v: &Value, path: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                let here = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if secret_key(k) {
                    out.push(here.clone());
                }
                secret_paths(c, &here, out);
            }
        }
        Value::Array(a) => {
            for (i, c) in a.iter().enumerate() {
                secret_paths(c, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// The bots in the export (version 1 only; secret-looking keys refuse the whole document, as upstream).
pub fn bots(p: &Path) -> Res<Vec<Value>> {
    let raw = read_str(p)?;
    let (v, repairs) = parse(&raw).map_err(|e| format!("{}: {e}", p.display()))?;
    check(&v).map_err(|e| match e.starts_with("not a ") {
        true => format!("{}: {e}", p.display()),
        false => e,
    })?;
    report_repairs(&repairs);
    let bots: Vec<Value> = dicts(&v["bots"]).into_iter().cloned().collect();
    if bots.is_empty() {
        return err("the export has no bots");
    }
    for (i, b) in bots.iter().enumerate() {
        if b["name"].as_str().unwrap_or("").trim().is_empty() {
            return err(format!("bots[{i}] needs a name"));
        }
    }
    Ok(bots)
}

/// The export document checked as upstream does: a grok-bot-export, version 1, no credential-looking keys.
/// The secret check runs on the parsed (so already repaired) document.
pub fn document(raw: &str) -> Res<(Value, Vec<String>)> {
    let (v, repairs) = parse(raw)?;
    check(&v)?;
    Ok((v, repairs))
}

fn check(v: &Value) -> Res<()> {
    if v["format"] != json!(FORMAT) && v.get("bots").is_none() {
        return err(format!("not a {FORMAT} document"));
    }
    if v.get("version").is_some_and(|x| *x != json!(1)) {
        return err(format!("unsupported {FORMAT} version {}", v["version"]));
    }
    let mut sec = vec![];
    secret_paths(v, "", &mut sec);
    if !sec.is_empty() {
        return err(format!(
            "the export carries credential-looking keys, so it was not read: {}",
            sec.into_iter().take(5).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

/// Parse an export as pasted: strict JSON first (unchanged behaviour); failing that, the tolerant path below.
/// Returns the document and one short line per repair applied (empty for strict JSON).
///
/// Tolerated, in this order:
/// 1. Fenced code blocks (```json / ``` / ~~~ lines, or longer fences; a one-line ```{…}``` too) with chatter
///    around them. One block that parses is taken as is. Several blocks are a reply split across messages: one
///    trailing line break is dropped from each (the fence line's own) and they are joined in order with nothing
///    between them, so a split inside a string or a number joins up; the join must parse. If two or more blocks
///    are each a complete export (an object with `format` or `bots`), the input is ambiguous and refused. Without
///    fences, the text from the first `{` to the last `}` is taken. An unclosed last fence runs to the end.
/// 2. Curly quotes (“ ” ‘ ’) used as JSON quotes, outside string values only; quotes inside strings stay.
/// 3. Trailing commas before `}` or `]`, outside string values.
pub fn parse(raw: &str) -> Res<(Value, Vec<String>)> {
    let strict = serde_json::from_str::<Value>(raw.trim());
    let first_err = match strict {
        Ok(v) => return Ok((v, vec![])),
        Err(e) => e.to_string(),
    };
    let mut repairs = vec![];
    let blocks = fenced_blocks(raw);
    let body = match blocks.len() {
        0 => {
            let (a, b) = (raw.find('{'), raw.rfind('}'));
            match (a, b) {
                (Some(a), Some(b)) if b > a => {
                    if !raw[..a].trim().is_empty() || !raw[b + 1..].trim().is_empty() {
                        repairs
                            .push("took the JSON object out of the surrounding text".to_string());
                    }
                    raw[a..=b].to_string()
                }
                _ => return err(first_err),
            }
        }
        1 => {
            repairs.push("took the JSON out of a fenced code block".to_string());
            blocks[0].clone()
        }
        n => {
            let whole = blocks.iter().filter(|b| is_whole_export(b)).count();
            if whole > 1 {
                return err(format!(
                    "{whole} of the {n} fenced code blocks are each a complete JSON document; \
                     paste only the export (ambiguous, nothing was read)"
                ));
            }
            if whole == 1 {
                // one complete document among fragments: the fragments are not part of it
                let b = blocks.iter().find(|b| is_whole_export(b)).unwrap();
                repairs.push(format!(
                    "took the one complete JSON document out of {n} fenced code blocks"
                ));
                b.clone()
            } else {
                repairs.push(format!(
                    "joined {n} fenced code blocks in order (a reply split across messages)"
                ));
                blocks
                    .iter()
                    .map(|b| {
                        b.strip_suffix("\r\n")
                            .or_else(|| b.strip_suffix('\n'))
                            .unwrap_or(b)
                    })
                    .collect::<String>()
            }
        }
    };
    let (v, more) = repair(&body).map_err(|e| format!("not valid JSON even after repairs: {e}"))?;
    repairs.extend(more);
    Ok((v, repairs))
}

/// The contents of every fenced code block, in order. A fence is a line starting with 3 or more ` or ~ (after
/// leading spaces); an opening fence may carry a language tag, and a closing fence is at least as long as the
/// opening one. A fence line that also ends with its fence (```json {…} ```) is a one-line block.
fn fenced_blocks(raw: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur: Option<(String, String)> = None; // (fence marker, body)
    for line in raw.split_inclusive('\n') {
        let t = line.trim_start();
        let marker = fence(t);
        match (&mut cur, marker) {
            (None, Some(m)) => {
                let rest = t[m.len()..].trim_end();
                match rest.strip_suffix(m.as_str()).map(str::trim_end) {
                    Some(inner) if inner.contains('{') => {
                        let lang = inner
                            .find(|c: char| !c.is_alphanumeric())
                            .unwrap_or(inner.len());
                        out.push(inner[lang..].trim().to_string());
                    }
                    _ => cur = Some((m, String::new())),
                }
            }
            (Some((open, _)), Some(m)) if m.starts_with(open.as_str()) && t.trim_end() == m => {
                out.push(cur.take().unwrap().1);
            }
            (Some((_, body)), _) => body.push_str(line),
            (None, None) => {}
        }
    }
    if let Some((_, body)) = cur {
        out.push(body);
    }
    out.retain(|b| !b.trim().is_empty());
    out
}

/// The fence a line starts with: 3 or more of the same ` or ~.
fn fence(t: &str) -> Option<String> {
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then(|| c.to_string().repeat(n))
}

/// A block that is a complete export on its own: an object with `format` or `bots`.
fn is_whole_export(b: &str) -> bool {
    repair(b).is_ok_and(|(v, _)| v.get("format").is_some() || v.get("bots").is_some())
}

/// Rewrite curly quotes used as JSON quotes and drop trailing commas, outside string values only; then parse.
fn repair(s: &str) -> Res<(Value, Vec<String>)> {
    let (mut curly, mut commas) = (0usize, 0usize);
    let mut out = String::with_capacity(s.len());
    let mut close: Option<char> = None; // the quote that ends the current string
    let mut esc = false;
    let cs: Vec<char> = s.trim().chars().collect();
    for (i, &c) in cs.iter().enumerate() {
        if let Some(q) = close {
            if esc {
                esc = false;
                out.push(c);
            } else if c == '\\' {
                esc = true;
                out.push(c);
            } else if (q == '"' && c == '"')
                || (q == '”' && matches!(c, '"' | '“' | '”') && ends_value(&cs[i + 1..]))
                || (q == '’' && matches!(c, '‘' | '’') && ends_value(&cs[i + 1..]))
            {
                // a curly-quoted string closes with a quote of its own kind (double: “ ” or ", single: ‘ ’)
                // that is followed by `:` `,` `}` `]` or the end; any other quote inside it is text
                close = None;
                if c != '"' {
                    curly += 1;
                }
                out.push('"');
            } else if c == '"' {
                // a straight quote inside a curly-quoted string is text
                out.push_str("\\\"");
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                close = Some('"');
                out.push('"');
            }
            '“' | '”' => {
                close = Some('”');
                curly += 1;
                out.push('"');
            }
            '‘' | '’' => {
                close = Some('’');
                curly += 1;
                out.push('"');
            }
            ',' if cs[i + 1..]
                .iter()
                .find(|x| !x.is_whitespace())
                .is_some_and(|x| *x == '}' || *x == ']') =>
            {
                commas += 1;
            }
            _ => out.push(c),
        }
    }
    let v: Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
    let mut r = vec![];
    if curly > 0 {
        r.push(format!("replaced {curly} curly quotes used as JSON quotes"));
    }
    if commas > 0 {
        r.push(format!("removed {commas} trailing commas"));
    }
    Ok((v, r))
}

fn ends_value(rest: &[char]) -> bool {
    rest.iter()
        .find(|x| !x.is_whitespace())
        .is_none_or(|x| matches!(x, ':' | ',' | '}' | ']'))
}

/// One stderr line per repair, once per process (the export is read several times per command).
fn report_repairs(repairs: &[String]) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if !repairs.is_empty() {
        ONCE.call_once(|| {
            for r in repairs {
                eprintln!("botport: repaired export: {r}");
            }
        });
    }
}

/// Upstream `_skill_markdown`, so our copy and the upstream import of one skill are byte-identical.
pub fn skill_markdown(s: &Value) -> String {
    let label = s["name"].as_str().unwrap_or("").trim();
    let desc = match s["description"].as_str().filter(|d| !d.trim().is_empty()) {
        Some(d) => d.split_whitespace().collect::<Vec<_>>().join(" "),
        None => format!("Imported Grok Bot skill: {label}"),
    };
    let desc: String = desc.chars().take(300).collect();
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n# {label}\n\n{}\n",
        slug(label),
        json!(desc),
        text(&s["instructions"])
    )
}

fn skills(bot: &Value) -> BTreeMap<String, String> {
    dicts(&bot["skills"])
        .into_iter()
        .filter(|s| {
            !s["name"].as_str().unwrap_or("").trim().is_empty()
                && !text(&s["instructions"]).is_empty()
        })
        .map(|s| (slug(s["name"].as_str().unwrap_or("")), skill_markdown(s)))
        .collect()
}

/// The first bot (an export may list several; Decisions in docs/USER-GUIDE.md).
fn first(p: &Path) -> Res<(Value, usize)> {
    let b = bots(p)?;
    Ok((b[0].clone(), b.len()))
}

pub fn read_model(p: &Path) -> Res<BotModel> {
    let (bot, n) = first(p)?;
    let fact = |t: &str, scope: &str| Fact {
        text: t.to_string(),
        tier: Some("core".into()),
        scope: Some(scope.into()),
        source: None,
        date: None,
        origin_side: None,
    };
    let mems = strings(&bot["memories"]);
    let users = strings(&bot["user_facts"]);
    let truncated = mems
        .iter()
        .chain(&users)
        .filter(|t| t.contains(TRUNCATED))
        .count();
    let mut memory: Vec<Fact> = mems.iter().map(|t| fact(t, "agent")).collect();
    memory.extend(users.iter().map(|t| fact(t, "user")));
    let skills = skills(&bot)
        .into_iter()
        .map(|(name, md)| Skill {
            name,
            description: None,
            files: [("SKILL.md".to_string(), md)].into(),
            binary_files: Default::default(),
        })
        .collect();
    let mut schedules = vec![];
    let mut routine_extras = serde_json::Map::new();
    for r in dicts(&bot["routines"]) {
        let name = r["name"].as_str().unwrap_or("").trim();
        let name = if name.is_empty() { "routine" } else { name };
        let trig = r["trigger"]
            .as_str()
            .unwrap_or("schedule")
            .trim()
            .to_lowercase();
        let cron = r["schedule_cron"].as_str().unwrap_or("").trim().to_string();
        let scheduled =
            SCHEDULE_TRIGGERS.contains(&trig.as_str()) && cron.split_whitespace().count() == 5;
        schedules.push(Schedule {
            name: name.to_string(),
            cron: scheduled.then_some(cron),
            tz: r["timezone"]
                .as_str()
                .filter(|t| !t.is_empty())
                .unwrap_or("UTC")
                .to_string(),
            kind: if scheduled {
                ScheduleKind::Scheduled
            } else {
                ScheduleKind::Event
            },
            action: text(&r["instruction"]),
            enabled: r["paused"] != json!(true),
        });
        let mut x = serde_json::Map::new();
        for k in [
            "trigger",
            "schedule_text",
            "trigger_details",
            "paused_reason",
            "skills",
        ] {
            match &r[k] {
                Value::Null => {}
                Value::String(s) if s.is_empty() => {}
                Value::Array(a) if a.is_empty() => {}
                v => {
                    x.insert(k.into(), v.clone());
                }
            }
        }
        routine_extras.insert(name.to_string(), x.into());
    }
    let (mut connectors, mut other_apps) = (vec![], vec![]);
    for a in dicts(&bot["apps"]) {
        let name = a["name"].as_str().unwrap_or("").trim();
        let url = a["mcp_url"].as_str().unwrap_or("").trim();
        if url.starts_with("https://") || url.starts_with("http://") {
            connectors.push(Connector {
                name: a["id"].as_str().map(app_id).unwrap_or_else(|| app_id(name)),
                kind: "mcp-http".into(),
                url: Some(url.into()),
                tools: vec![],
            });
        } else if !name.is_empty() {
            other_apps.push(name.to_string());
        }
    }
    let mut extra = json!({"format": FORMAT});
    for (k, v) in [
        (
            "title",
            bot["title"]
                .as_str()
                .filter(|t| !t.trim().is_empty())
                .map(|t| json!(t.trim())),
        ),
        (
            "routines",
            (!routine_extras.is_empty()).then(|| routine_extras.into()),
        ),
        (
            "apps_without_url",
            (!other_apps.is_empty()).then(|| json!(other_apps)),
        ),
        (
            "truncated_memories",
            (truncated > 0).then(|| json!(truncated)),
        ),
        ("other_bots", (n > 1).then(|| json!(n - 1))),
    ] {
        if let Some(v) = v {
            extra[k] = v;
        }
    }
    Ok(BotModel {
        schema: format!("{SCHEMA_FAMILY}/{SCHEMA_MAJOR}"),
        persona: Persona {
            name: bot["name"].as_str().unwrap_or("").trim().to_string(),
            instructions: text(&bot["instructions"]),
        },
        memory,
        // memories as `- fact` lines: what a writer renders from `memory` anyway
        memory_text: None,
        skills,
        schedules,
        connectors,
        credentials: vec![],
        history: None,
        manual: vec![],
        extras: [(super::ID.to_string(), extra)].into_iter().collect(),
    })
}

/// The export as a `Bundle` (skills, memory as MEMORY.md lines, all facts for echo prevention); the sync
/// digest covers the export file itself.
pub fn load_bundle(p: &Path) -> Res<Bundle> {
    let (bot, _) = first(p)?;
    let mems = strings(&bot["memories"]);
    let mut facts = mems.clone();
    facts.extend(strings(&bot["user_facts"]));
    let memory_md = (!mems.is_empty()).then(|| mems.iter().map(|m| format!("- {m}\n")).collect());
    let fname = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "grok-bot-export.json".into());
    Ok(Bundle {
        root: p.to_path_buf(),
        agent_id: String::new(),
        name: bot["name"].as_str().unwrap_or("").trim().to_string(),
        skills: skills(&bot)
            .into_iter()
            .map(|(n, md)| (n, [("SKILL.md".to_string(), md.into_bytes())].into()))
            .collect(),
        memory_md,
        facts,
        excluded: Default::default(),
        read: [(
            fname,
            sha256_hex(&std::fs::read(p).map_err(|e| e.to_string())?),
        )]
        .into(),
        instructions: text(&bot["instructions"]),
        user_facts: strings(&bot["user_facts"]),
    })
}

/// Truncated-memory markers in the export (memories and user facts that end cut off).
pub fn truncated_count(p: &Path) -> Res<usize> {
    let (bot, _) = first(p)?;
    Ok(strings(&bot["memories"])
        .iter()
        .chain(&strings(&bot["user_facts"]))
        .filter(|t| t.contains(TRUNCATED))
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{"format": "grok-bot-export", "version": 1, "bots": [{"name": "Nova", "memories": ["it’s “quoted”", "a, b"]}]}"#;

    fn ok(raw: &str) -> (Value, Vec<String>) {
        document(raw).unwrap_or_else(|e| panic!("{e}\n---\n{raw}"))
    }

    #[test]
    fn strict_json_has_no_repairs_and_same_value() {
        let (v, r) = ok(DOC);
        assert!(r.is_empty());
        assert_eq!(v, serde_json::from_str::<Value>(DOC).unwrap());
    }

    #[test]
    fn fenced_block_with_chatter() {
        for fence in ["```json", "```", "~~~"] {
            let close = &fence[..3];
            let raw = format!("Here is my export:\n\n{fence}\n{DOC}\n{close}\n\nLet me know!\n");
            let (v, r) = ok(&raw);
            assert_eq!(v, ok(DOC).0);
            assert_eq!(r, ["took the JSON out of a fenced code block"]);
        }
        // no fence: the object is cut out of the text
        let (_, r) = ok(&format!("Sure! {DOC} Done."));
        assert_eq!(r, ["took the JSON object out of the surrounding text"]);
    }

    #[test]
    fn reply_split_across_messages_is_joined_in_order() {
        let (a, b) = DOC.split_at(40);
        let raw = format!("Part 1:\n```json\n{a}\n```\n(continued)\n```\n{b}\n```\n");
        let (v, r) = ok(&raw);
        assert_eq!(v, ok(DOC).0);
        assert!(r[0].starts_with("joined 2 fenced code blocks"), "{r:?}");
        // an unclosed last fence (message cut off) runs to the end
        let raw = format!("```json\n{a}\n```\n```json\n{b}");
        assert_eq!(ok(&raw).0, ok(DOC).0);
        // fragments in the wrong order do not parse: refused
        let raw = format!("```\n{b}\n```\n```\n{a}\n```\n");
        assert!(document(&raw).is_err());
    }

    #[test]
    fn several_complete_documents_are_ambiguous() {
        let raw = format!("```json\n{DOC}\n```\nor\n```json\n{DOC}\n```\n");
        let e = document(&raw).unwrap_err();
        assert!(e.contains("ambiguous"), "{e}");
    }

    #[test]
    fn curly_quotes_as_json_quotes() {
        let raw = r#"{“format”: “grok-bot-export”, ‘version’: 1, “bots”: [{“name”: “Nova”, “memories”: [“it’s a "test"”, “ok”]}]}"#;
        let (v, r) = ok(raw);
        assert_eq!(v["bots"][0]["memories"][0], json!("it’s a \"test\""));
        assert_eq!(v["version"], json!(1));
        assert!(
            r[0].starts_with("replaced") && r[0].contains("curly quotes"),
            "{r:?}"
        );
        // curly quotes inside straight-quoted strings are text and untouched
        assert_eq!(ok(DOC).0["bots"][0]["memories"][0], json!("it’s “quoted”"));
    }

    #[test]
    fn trailing_commas_removed_outside_strings() {
        let raw = DOC
            .replace("\"a, b\"]", "\"a, b\",]")
            .replace("]}]}", "],},]}");
        let (v, r) = ok(&raw);
        assert_eq!(v, ok(DOC).0);
        assert_eq!(r, ["removed 3 trailing commas"]);
        // a ", ]" inside a string value is text
        let raw = DOC.replace("a, b", "x, ]");
        assert_eq!(ok(&raw).0["bots"][0]["memories"][1], json!("x, ]"));
    }

    #[test]
    fn secret_inside_fenced_block_is_still_refused() {
        let doc = DOC.replace(
            "\"name\": \"Nova\"",
            "\"name\": \"Nova\", \"api_key\": \"x\"",
        );
        let raw = format!(
            "Here you go\n```json\n{}\n```\n",
            doc.replace("]}]}", "],}]}")
        );
        let e = document(&raw).unwrap_err();
        assert!(
            e.contains("credential-looking") && e.contains("api_key"),
            "{e}"
        );
        let raw = format!("```json\n{}\n```", doc.replace('"', "“"));
        let e = document(&raw).unwrap_err();
        assert!(e.contains("credential-looking"), "{e}");
    }

    #[test]
    fn curly_closer_matches_its_opener() {
        // smart punctuation inside a “…” value: the ’ after cats is text, not the end of the string
        let raw = "{“format”: “grok-bot-export”, “version”: 1, “bots”: [{“name”: “Nova”, “memories”: [“I like ‘cats’, ‘dogs’”, “ok”]}]}";
        let (v, _) = ok(raw);
        assert_eq!(
            v["bots"][0]["memories"],
            json!(["I like ‘cats’, ‘dogs’", "ok"])
        );
        // a ” inside a “…” value that does not end it stays text
        let raw = "{“format”: “grok-bot-export”, “bots”: [{“name”: “say “hi” now”}]}";
        assert_eq!(ok(raw).0["bots"][0]["name"], json!("say “hi” now"));
        // a ‘…’ string ends only on a single quote
        let raw = "{‘format’: ‘grok-bot-export’, ‘bots’: [{‘name’: ‘a ”, b’}]}";
        assert_eq!(ok(raw).0["bots"][0]["name"], json!("a ”, b"));
    }

    #[test]
    fn split_inside_a_string_or_a_number_joins_up() {
        let doc = r#"{"format": "grok-bot-export", "version": 1, "bots": [{"name": "Nova", "age_days": 12345, "memories": ["a long memory that the bot cut in the middle of", "ok"]}]}"#;
        for at in [doc.find("cut in").unwrap(), doc.find("345").unwrap()] {
            let (a, b) = doc.split_at(at);
            for nl in ["\n", "\r\n"] {
                let raw = format!("```json{nl}{a}{nl}```{nl}(more){nl}```{nl}{b}{nl}```{nl}");
                let (v, r) = ok(&raw);
                assert_eq!(v, serde_json::from_str::<Value>(doc).unwrap(), "{raw}");
                assert!(r[0].starts_with("joined 2"), "{r:?}");
            }
        }
    }

    #[test]
    fn a_non_export_block_is_not_a_complete_document() {
        let raw = format!("For example:\n```\n42\n```\nThe export:\n```json\n{DOC}\n```\n");
        let (v, r) = ok(&raw);
        assert_eq!(v, ok(DOC).0);
        assert_eq!(
            r,
            ["took the one complete JSON document out of 2 fenced code blocks"]
        );
    }

    #[test]
    fn long_and_one_line_fences() {
        let raw = format!("Here:\n````json\n{DOC}\n````\nAnything else? ```\n");
        assert_eq!(ok(&raw).0, ok(DOC).0);
        let raw = format!("Here: \n```json {DOC} ```\nHope that helps!\n");
        assert_eq!(ok(&raw).0, ok(DOC).0);
        // a ``` inside a ```` block is text
        let raw = format!("````\n{}\n````\n", DOC.replace("a, b", "```"));
        assert_eq!(ok(&raw).0["bots"][0]["memories"][1], json!("```"));
    }

    #[test]
    fn garbage_is_refused() {
        assert!(document("no json here").is_err());
        assert!(document("```\n{nope\n```").is_err());
    }
}

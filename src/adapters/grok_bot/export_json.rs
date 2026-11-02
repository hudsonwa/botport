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
/// 2. Trailing commas before `}` or `]`, outside string values.
///
/// Curly quotes are never repaired: if the text still does not parse and holds one (“ ” ‘ ’), it is refused with
/// [`SMART_QUOTES`]. Curly quotes inside straight-quoted strings are text and parse strictly. After any repair the
/// item counts under memories/user_facts/skills/routines/schedules must match a structural count of the text.
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
    let (v, more) = repair(&body)?;
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

/// The refusal for a paste that only parses if its curly quotes were guessed back into JSON quotes.
pub const SMART_QUOTES: &str = "the export was mangled by the chat app (smart quotes); ask the bot to re-send it as a file or a plain code block";

/// Keys whose arrays are counted before and after a repair (memories, skills, routines/schedules).
const COUNTED: [&str; 5] = ["memories", "user_facts", "skills", "routines", "schedules"];

/// Drop trailing commas before `}` or `]` outside (straight-quoted) strings; then parse. Curly quotes are never
/// rewritten: a text that still fails to parse and holds one is refused with [`SMART_QUOTES`]. The parsed result
/// must hold as many items under each counted key as a structural count of the text, else it is refused.
fn repair(s: &str) -> Res<(Value, Vec<String>)> {
    let mut commas = 0usize;
    let mut out = String::with_capacity(s.len());
    let (mut in_str, mut esc) = (false, false);
    let cs: Vec<char> = s.trim().chars().collect();
    for (i, &c) in cs.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            out.push(c);
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
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
    let v: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(_) if s.contains(['“', '”', '‘', '’']) => return Err(SMART_QUOTES.into()),
        Err(e) => return Err(format!("not valid JSON even after repairs: {e}")),
    };
    let (want, got) = (pre_count(s), parsed_count(&v));
    if want != got {
        return Err(format!(
            "the repaired export does not hold what the pasted text does (items per list, pasted {want:?}, read {got:?}); \
             nothing was read: ask the bot to re-send it as a file or a plain code block"
        ));
    }
    let mut r = vec![];
    if commas > 0 {
        r.push(format!("removed {commas} trailing commas"));
    }
    Ok((v, r))
}

/// Items in every array under a counted key, from the raw text: one per value that starts in that array (so a
/// trailing comma adds none). It tracks only brackets, commas and straight-quoted strings, never the repair.
fn pre_count(s: &str) -> BTreeMap<String, usize> {
    let mut n: BTreeMap<String, usize> = BTreeMap::new();
    // per open container: (is array, counted key name, waiting for a value)
    let mut stack: Vec<(bool, Option<String>, bool)> = vec![];
    let (mut key, mut last) = (None::<String>, String::new());
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if let Some((true, Some(k), want @ true)) = stack.last_mut() {
            if c != ']' && c != ',' {
                *want = false;
                *n.entry(k.clone()).or_default() += 1;
            }
        }
        match c {
            '"' => {
                let mut j = i + 1;
                let mut t = String::new();
                while j < cs.len() && cs[j] != '"' {
                    if cs[j] == '\\' {
                        j += 1;
                    }
                    if let Some(&x) = cs.get(j) {
                        t.push(x);
                    }
                    j += 1;
                }
                last = t;
                i = j;
            }
            ':' => key = Some(std::mem::take(&mut last)),
            '[' => {
                let k = key.take().filter(|k| COUNTED.contains(&k.as_str()));
                if let Some(k) = &k {
                    n.entry(k.clone()).or_default();
                }
                stack.push((true, k, true));
            }
            '{' => {
                key = None;
                stack.push((false, None, false));
            }
            ']' | '}' => {
                stack.pop();
                key = None;
            }
            ',' => {
                key = None;
                if let Some((true, _, want)) = stack.last_mut() {
                    *want = true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

/// The same count from the parsed document.
fn parsed_count(v: &Value) -> BTreeMap<String, usize> {
    fn walk(v: &Value, n: &mut BTreeMap<String, usize>) {
        match v {
            Value::Object(m) => {
                for (k, c) in m {
                    if let (true, Some(a)) = (COUNTED.contains(&k.as_str()), c.as_array()) {
                        *n.entry(k.clone()).or_default() += a.len();
                    }
                    walk(c, n);
                }
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, n)),
            _ => {}
        }
    }
    let mut n = BTreeMap::new();
    walk(v, &mut n);
    n
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
    fn curly_quotes_are_refused_not_repaired() {
        let raw =
            r#"{“format”: “grok-bot-export”, “bots”: [{“name”: “Nova”, “memories”: [“a”, “b”]}]}"#;
        assert_eq!(document(raw).unwrap_err(), SMART_QUOTES);
        // even inside a fenced block with a trailing comma: the message is the plain refusal, not a JSON error
        let raw = format!("Here:\n```json\n{}\n```\n", raw.replace("]}]}", "],}]}"));
        assert_eq!(document(&raw).unwrap_err(), SMART_QUOTES);
        // curly quotes inside straight-quoted strings are text and untouched, also after a repair
        assert_eq!(ok(DOC).0["bots"][0]["memories"][0], json!("it’s “quoted”"));
        let (v, r) = ok(&DOC.replace("]}]}", "],}]}"));
        assert_eq!(v, ok(DOC).0);
        assert_eq!(r, ["removed 1 trailing commas"]);
    }

    #[test]
    fn structural_count_matches_the_parsed_document() {
        let raw = r#"{"bots": [{"memories": ["a, ]", "b",], "skills": [{"name": "x", "routines": []},], "user_facts": [[1, 2], "c"]}]}"#;
        let want: BTreeMap<String, usize> = [
            ("memories", 2),
            ("skills", 1),
            ("routines", 0),
            ("user_facts", 2),
        ]
        .map(|(k, n)| (k.to_string(), n))
        .into();
        assert_eq!(pre_count(raw), want);
        let (v, _) = ok(raw);
        assert_eq!(parsed_count(&v), want);
        // a value that starts in the list but is lost would show as a mismatch
        assert_ne!(
            pre_count(r#"{"memories": ["a", "b"]}"#),
            parsed_count(&json!({"memories": ["ab"]}))
        );
    }

    /// Every input the audits of rounds 1-3 used against the tolerant parser (tests/fixtures/paste-cases;
    /// the token cases are in tests/cli.rs): each is refused, or read with exactly its values.
    #[test]
    fn audit_counterexamples_refuse_or_parse_exactly() {
        const NOVA: &str = "a long memory that the bot cut in the middle of";
        let refused: &[(&str, &str)] = &[
            ("05-pretty-fence-line", "not valid JSON"),
            ("10-secret-key-split", "not valid JSON"),
            ("13-two", "ambiguous"),
            ("20-chatter-braces", "not valid JSON"),
            ("24-key-in-frag2", "credential-looking"),
            ("38-key-split-mid", "credential-looking"),
        ];
        let smart = [
            "01-smart-array",
            "02-curly-commabrace",
            "11-secret-curly",
            "17-smart-obj",
            "19-mixed",
            "27-curly-escaped",
            "28-straight-in-curly",
            "29-curly-inner-pair-comma",
            "30-curly-inner-comma",
            "31-curly-commas",
            "35-single-apostrophe",
            "36-curly-double-close",
            "37-curly-plain",
            "39-curly-list-in-value",
            "40-escaped-straight-in-curly",
            "41-curly-sep-comma",
            "42-curly-sep-trailing",
            "43-user-guide-example",
            "44-singles-in-double",
            "45-single-nested",
            "46-single-plural-possessive",
            "47-mixed-straight-in-curly",
            "48-mixed-key-mangle",
            "50-apostrophes",
            "51-escaped-straight-in-curly",
            "52-double-in-single",
            "53-curly-trailing",
            "63-mixed-key-swallows-memories",
            "64-skills-split",
        ];
        let read: &[(&str, Value)] = &[
            (
                "03-escapes",
                json!(["say \"hi\",}", "back\\", "x ,]", "q\\\""]),
            ),
            (
                "04-fence-in-string",
                json!(["use ```json\n{}\n``` blocks", "```"]),
            ),
            ("06-crlf", json!(["a\r\nb", "c"])),
            ("07-split-midstring", json!([NOVA, "b"])),
            ("08-split-between", json!([NOVA, "b"])),
            ("12-whole-plus-fragment", json!([NOVA, "b"])),
            ("14-doc-plus-scalar", json!([NOVA, "b"])),
            ("15-four-backticks", json!([NOVA, "b"])),
            ("16-oneline-fence", json!([NOVA, "b"])),
            (
                "18-straight-with-curly",
                json!(["it’s “quoted”, ok”", "‘x’,"]),
            ),
            ("21-backslash", json!(["C:\\dir\\", ",}"])),
            (
                "32-crlf-split",
                json!(["a long memory that the bot cut in the middle", "b"]),
            ),
            ("33-fence-split-ambig", json!(["see ```jsonhere", "b"])),
            ("34-whole-plus-object", json!(["ok"])),
            (
                "49-curly-in-straight",
                json!(["it’s “quoted”, ok", "‘x’, ”y“", "don’t"]),
            ),
            ("54-nested-fence", json!(["use ```json fences", "~~~ too"])),
            ("55-fence-chars-in-value", json!(["a ``` b", "c"])),
            ("56-split-after-fence-in-value", json!(["ends ```", "x"])),
        ];
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/paste-cases");
        let case = |n: &str| std::fs::read_to_string(dir.join(format!("{n}.txt"))).unwrap();
        for (n, want) in refused {
            let e = document(&case(n)).unwrap_err();
            assert!(e.contains(want), "{n}: {e}");
        }
        for n in smart {
            assert_eq!(document(&case(n)).unwrap_err(), SMART_QUOTES, "{n}");
        }
        for (n, want) in read {
            let (v, _) = document(&case(n)).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert_eq!(v["bots"].as_array().unwrap().len(), 1, "{n}");
            assert_eq!(v["bots"][0]["name"], json!("Nova"), "{n}");
            assert_eq!(v["bots"][0]["memories"], *want, "{n}");
        }
        let (v, _) = document(&case("09-split-midnumber")).unwrap();
        assert_eq!(v["bots"][0]["n"], json!(12345));
        let n = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(
            n,
            refused.len() + smart.len() + read.len() + 1,
            "a case file without an expectation"
        );
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
        let raw = format!(
            "```json\n{}\n```",
            doc.replace("“quoted”", "quoted").replace('"', "“")
        );
        assert_eq!(document(&raw).unwrap_err(), SMART_QUOTES);
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

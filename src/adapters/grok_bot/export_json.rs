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
    let v: Value = serde_json::from_str(raw.trim()).map_err(|e| format!("{}: {e}", p.display()))?;
    if v["format"] != json!(FORMAT) && v.get("bots").is_none() {
        return err(format!("{}: not a {FORMAT} document", p.display()));
    }
    if v.get("version").is_some_and(|x| *x != json!(1)) {
        return err(format!("unsupported {FORMAT} version {}", v["version"]));
    }
    let mut sec = vec![];
    secret_paths(&v, "", &mut sec);
    if !sec.is_empty() {
        return err(format!(
            "the export carries credential-looking keys, so it was not read: {}",
            sec.into_iter().take(5).collect::<Vec<_>>().join(", ")
        ));
    }
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

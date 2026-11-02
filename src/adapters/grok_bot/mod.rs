//! Grok Bot adapter (docs: `adapters/grok-bot`): export bundle read/write/verify, and the `Adapter` impl.
pub mod bundle;
pub mod describe;
pub mod export_json;

use crate::core::adapter::{Adapter, Source};
use crate::core::model::{
    BotModel, Connector, CredentialRef, Fact, ManualItem, Persona, Schedule, ScheduleKind, Skill,
    SCHEMA_FAMILY, SCHEMA_MAJOR,
};
use crate::core::secrets;
use crate::core::util::*;
use serde_json::{json, Value};
use std::path::Path;

pub const ID: &str = "grok-bot";

pub struct GrokBot;

/// Schedules, connectors and credential names the bot described, plus schedules it said run elsewhere.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Described {
    pub schedules: Vec<Schedule>,
    pub connectors: Vec<Connector>,
    pub credentials: Vec<CredentialRef>,
    /// names of schedules that run on another machine (not on Grok)
    pub elsewhere: Vec<String>,
    /// lines of the description nobody could classify
    pub unparsed: usize,
    /// export apps with no MCP url (logins stay on the Grok side)
    pub apps_without_url: Vec<String>,
    /// (item, what to do by hand) for schedules/connectors that looked like they hold a secret: never imported
    pub manual: Vec<(String, String)>,
}

/// describe.json (see describe.rs) -> model parts. Names only for credentials (re-scanned).
pub fn from_describe(v: &Value) -> Described {
    let mut d = Described::default();
    let arr = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    let st = |x: &Value, k: &str| x[k].as_str().unwrap_or("").to_string();
    for s in arr("schedules") {
        if s["side"] == "other" {
            d.elsewhere.push(st(&s, "name"));
            continue;
        }
        d.schedules.push(Schedule {
            name: st(&s, "name"),
            cron: s["cron"].as_str().map(String::from),
            tz: st(&s, "timezone"),
            kind: ScheduleKind::Scheduled,
            action: st(&s, "prompt"),
            enabled: !s["paused"].as_bool().unwrap_or(false),
        });
    }
    for c in arr("connectors") {
        let url = c["url"].as_str().map(String::from);
        let http = url
            .as_deref()
            .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"));
        let kind = match c["kind"].as_str().unwrap_or("unknown") {
            "mcp" if http => "mcp-http",
            k => k,
        };
        d.connectors.push(Connector {
            name: st(&c, "name"),
            kind: kind.into(),
            url,
            tools: c["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(String::from))
                .collect(),
        });
    }
    // Names only, re-scanned: a hand-edited or older describe.json may carry `where` or a value-like name.
    for c in arr("credentials") {
        let name = st(&c, "name");
        match secrets::credential_name(&name) {
            None => d.credentials.push(CredentialRef::new(name)),
            Some(r) => d.manual.push((
                "credential (name not shown)".into(),
                format!("not imported: {r}; its text was not copied, set it up by hand"),
            )),
        }
    }
    d.unparsed = arr("unparsed").len();
    for m in arr("manual") {
        d.manual.push((
            format!("{} {}", st(&m, "kind"), st(&m, "name")),
            format!(
                "not imported: line {} looks like it holds a secret ({}); its text was not copied, set it up by hand",
                m["line"], st(&m, "reason")
            ),
        ));
    }
    d
}

/// The description beside an export: `normalized/describe.json` (bundle), else a `NOT_CAPTURED.md` in the bundle
/// dir; for a grok-bot-export file, a `describe.json` next to it.
fn description(src: &Path) -> Res<Option<Value>> {
    let (dj, nc) = if src.is_file() {
        // a pasted export has no description of its own: never pick one up from its dir
        if src
            .file_name()
            .is_some_and(|n| n == "pasted-grok-bot-export.json")
        {
            return Ok(None);
        }
        (src.with_file_name("describe.json"), None)
    } else {
        (
            src.join("normalized/describe.json"),
            Some(src.join("NOT_CAPTURED.md")),
        )
    };
    if dj.is_file() {
        return serde_json::from_slice(&read(&dj)?)
            .map(Some)
            .map_err(|e| format!("{}: {e}", dj.display()));
    }
    match nc.filter(|p| p.is_file()) {
        Some(p) => Ok(Some(describe::parse(&read_str(&p)?))),
        None => Ok(None),
    }
}

/// Everything the source says about schedules/connectors/credentials: the export's routines/apps first, then the
/// bot's description, deduped by name (the export wins).
pub fn described(src: &Path) -> Res<Described> {
    let mut d = match description(src)? {
        Some(v) => from_describe(&v),
        None => Described::default(),
    };
    if src.is_file() {
        let m = export_json::read_model(src)?;
        let apps = m.extras[ID]["apps_without_url"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(String::from))
            .collect();
        let mut e = Described {
            apps_without_url: apps,
            schedules: m.schedules,
            connectors: m.connectors,
            credentials: m.credentials,
            ..Described::default()
        };
        e.schedules.append(&mut d.schedules);
        e.connectors.append(&mut d.connectors);
        e.credentials.append(&mut d.credentials);
        e.elsewhere = d.elsewhere;
        e.manual = d.manual;
        e.unparsed = d.unparsed;
        d = e;
    }
    dedup(&mut d.schedules, |x| x.name.clone());
    dedup(&mut d.connectors, |x| x.name.clone());
    dedup(&mut d.credentials, |x| x.name.clone());
    Ok(d)
}

fn dedup<T>(v: &mut Vec<T>, key: impl Fn(&T) -> String) {
    let mut seen = std::collections::BTreeSet::new();
    v.retain(|x| seen.insert(key(x)));
}

/// What the description names but nothing can import: secret-looking entries, schedules that run on another
/// machine, apps without an MCP url (names and reasons only).
fn manual_items(d: &Described) -> Vec<ManualItem> {
    let mut out: Vec<ManualItem> = d
        .manual
        .iter()
        .map(|(item, todo)| ManualItem::new(item, todo))
        .collect();
    for n in &d.elsewhere {
        out.push(ManualItem::new(
            format!("schedule {n}"),
            "runs on another machine, not on Grok: move it by hand if it should",
        ));
    }
    for a in &d.apps_without_url {
        out.push(ManualItem::new(
            format!("connector {a}"),
            "app without an MCP url (its login stays on the Grok side): connect it by hand",
        ));
    }
    out
}

/// The model parts written back as a description `describe::parse` reads (events and paused flags as prose).
fn render_description(m: &BotModel) -> String {
    let mut s = String::from("# Not captured by the export\n\n## The bot's description\n");
    let one = |t: &str| {
        t.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace('"', "'")
    };
    if !m.schedules.is_empty() {
        s += "\n### Schedules\n";
        for x in &m.schedules {
            let paused = if x.enabled { "" } else { " (paused)" };
            match &x.cron {
                Some(c) => {
                    let tz = if x.tz.contains('/') || x.tz.is_empty() {
                        format!(" {}", x.tz)
                    } else {
                        format!(" at {}", x.tz)
                    };
                    let prompt = if x.action.is_empty() {
                        String::new()
                    } else {
                        format!(" — prompt: \"{}\"", one(&x.action))
                    };
                    s += &format!("- **{}**: `{c}`{}{paused}{prompt}\n", x.name, tz.trim_end());
                }
                None => s += &format!("- **{}**: on an event{paused}\n", x.name),
            }
        }
    }
    if !m.connectors.is_empty() {
        s += "\n### Connectors\n";
        for c in &m.connectors {
            let what = match (c.kind.as_str(), &c.url) {
                ("mcp-http" | "mcp", Some(u)) => format!("MCP server at {u}"),
                ("mcp-http" | "mcp", None) => "MCP server".into(),
                ("builtin", _) => "built-in".into(),
                (k, Some(u)) => format!("{k} at {u}"),
                (k, None) => k.to_string(),
            };
            let tools = if c.tools.is_empty() {
                String::new()
            } else {
                format!(" (tools: {})", c.tools.join(", "))
            };
            s += &format!("- **{}**: {what}{tools}\n", c.name);
        }
    }
    if !m.credentials.is_empty() {
        s += "\n### Credentials\n";
        for c in &m.credentials {
            s += &format!("- `{}`\n", c.name);
        }
    }
    s
}

impl Adapter for GrokBot {
    fn id(&self) -> &'static str {
        ID
    }

    fn read(&self, src: &Path) -> Res<BotModel> {
        self.read_source(src).map(|(m, _)| m)
    }

    /// A grok-bot-export file, else a bundle directory (exported, or a hand export). Persona instructions come
    /// only from an export file: a bundle's profile.json is kept verbatim in `extras` (a Grok box's profile carries
    /// no live instructions, and the import never wrote them to SOUL.md).
    fn read_source(&self, src: &Path) -> Res<(BotModel, Source)> {
        if export_json::is_export(src) {
            let mut m = export_json::read_model(src)?;
            let d = described(src)?;
            m.manual = manual_items(&d);
            (m.schedules, m.connectors, m.credentials) = (d.schedules, d.connectors, d.credentials);
            let b = export_json::load_bundle(src)?;
            let s = Source {
                path: src.to_path_buf(),
                id: b.agent_id,
                files_read: b.read,
                excluded: b.excluded,
            };
            return Ok((m, s));
        }
        let b = bundle::load(src)?;
        let base = if src.join("manifest.json").is_file() {
            src.join("normalized")
        } else {
            src.to_path_buf()
        };
        let profile: Value = match base.join("profile.json").is_file() {
            true => serde_json::from_slice(&read(&base.join("profile.json"))?)
                .map_err(|e| format!("profile.json: {e}"))?,
            false => json!({}),
        };
        let memory = b
            .facts
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| Fact {
                text: l.strip_prefix("- ").unwrap_or(l).to_string(),
                tier: Some("core".into()),
                scope: None,
                source: None,
                date: None,
                origin_side: None,
            })
            .collect();
        let skills = b
            .skills
            .iter()
            .map(|(n, files)| Skill::from_bytes(n, files))
            .collect();
        let mut extra = json!({"profile": profile});
        if !b.agent_id.is_empty() {
            extra["agent_id"] = json!(b.agent_id);
        }
        let d = described(src)?;
        if d.unparsed > 0 {
            extra["describe_unparsed"] = json!(d.unparsed);
        }
        let m = BotModel {
            schema: format!("{SCHEMA_FAMILY}/{SCHEMA_MAJOR}"),
            persona: Persona {
                name: b.name.clone(),
                instructions: String::new(),
            },
            memory,
            memory_text: b.memory_md.clone(),
            skills,
            manual: manual_items(&d),
            schedules: d.schedules,
            connectors: d.connectors,
            credentials: d.credentials,
            history: None,
            extras: [(ID.to_string(), extra)].into_iter().collect(),
        };
        let s = Source {
            path: src.to_path_buf(),
            id: b.agent_id,
            files_read: b.read,
            excluded: b.excluded,
        };
        Ok((m, s))
    }

    /// Writes the legacy hand-export layout (profile.json, MEMORY.md, skills/<name>/...), which `read` loads back.
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>> {
        let mut profile = m
            .extras
            .get(ID)
            .and_then(|e| e.get("profile"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        profile["name"] = json!(m.persona.name);
        if !m.persona.instructions.is_empty() {
            profile["instructions"] = json!(m.persona.instructions);
        }
        let mut out = vec![(
            "profile.json".to_string(),
            json_sorted(&profile).into_bytes(),
        )];
        let mut md = format!("# {} memory\n\n", m.persona.name);
        for f in &m.memory {
            md += &format!("- {}\n", f.text);
        }
        out.push(("MEMORY.md".into(), md.into_bytes()));
        if !(m.schedules.is_empty() && m.connectors.is_empty() && m.credentials.is_empty()) {
            out.push(("NOT_CAPTURED.md".into(), render_description(m).into_bytes()));
        }
        for s in &m.skills {
            for (rel, body) in s.bytes() {
                out.push((format!("skills/{}/{rel}", s.name), body));
            }
        }
        for (rel, b) in &out {
            write_atomic(&dest.join(rel), b, None)?;
        }
        Ok(out.into_iter().map(|(r, _)| r).collect())
    }
}

#[cfg(test)]
mod seeded_tests {
    use super::*;

    /// A describe.json (hand-edited or older) is re-scanned: `where` is dropped and a value-like name is
    /// a manual item without the name.
    #[test]
    fn credentials_from_describe_are_names_only_and_rescanned() {
        let d = from_describe(&serde_json::json!({"credentials": [
            {"name": "VAULT_PW", "where": "kept in zqhunter vault"},
            {"name": "PW_zqhunterpass", "where": null}
        ]}));
        let names: Vec<&str> = d.credentials.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["VAULT_PW"]);
        let all = format!("{:?}", d.manual);
        assert!(!all.contains("zqhunter"), "{all}");
        assert_eq!(d.manual.len(), 1, "{all}");
        let m = BotModel {
            credentials: d.credentials,
            ..BotModel::from_json(include_str!("../../../tests/fixtures/model/minimal.json"))
                .unwrap()
        };
        assert!(!render_description(&m).contains("zqhunter"));
        assert!(!serde_json::to_string(&m).unwrap().contains("where"));
    }

    /// The seeded fake secret (tests/describe_sync.rs) never reaches the model JSON or the NOT_CAPTURED.md written back.
    #[test]
    fn a_seeded_secret_reaches_neither_the_model_nor_not_captured() {
        let t = std::env::temp_dir().join(format!("g2h-seeded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("profile.json"), r#"{"name":"Nova"}"#).unwrap();
        std::fs::write(t.join("MEMORY.md"), "# Nova memory\n").unwrap();
        std::fs::write(
            t.join("NOT_CAPTURED.md"),
            include_str!("../../../tests/fixtures/describe/seeded.md"),
        )
        .unwrap();
        let m = GrokBot.read(&t).unwrap();
        let j = serde_json::to_string(&m).unwrap();
        let nc = render_description(&m);
        for s in include_str!("../../../tests/fixtures/describe/seeded-secrets.txt")
            .lines()
            .filter(|l| !l.is_empty())
        {
            assert!(!j.contains(s), "{s} in model: {j}");
            assert!(!nc.contains(s), "{s} in NOT_CAPTURED.md: {nc}");
        }
        assert!(nc.contains("MY_KEY"), "{nc}");
        std::fs::remove_dir_all(&t).unwrap();
    }
}

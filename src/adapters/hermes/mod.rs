//! Hermes adapter: port spec, plan/apply/sync (`sync`), two-way export/drift (`twoway`), and the `Adapter` impl.
pub mod spec;
pub mod sync;
pub mod twoway;

use crate::core::adapter::{Adapter, Applied, Plan, Source, Target};
use crate::core::model::{
    BotModel, Connector, CredentialRef, Fact, ManualItem, Persona, Schedule, ScheduleKind, Skill,
    SCHEMA_FAMILY, SCHEMA_MAJOR,
};
use crate::core::util::*;
use serde_json::json;
use serde_yaml::Value as Y;
use std::path::{Path, PathBuf};

pub const ID: &str = "hermes";

/// A Hermes profile directory (SOUL.md, memories/, skills/, config.yaml, cron/jobs.json, botport-manual.md).
pub struct HermesProfile;

/// How plan/apply treat the profile: the port spec (ownership templates) and how cron is managed.
pub struct Opts {
    pub spec: spec::Spec,
    /// profile name (what `hermes -p` takes)
    pub profile: String,
    /// cron is managed through the hermes CLI (only for ~/.hermes, or an explicit --hermes-bin)
    pub real_home: bool,
    pub hermes_bin: String,
    /// HERMES_HOME for the hermes CLI when it is not ~/.hermes (None = the CLI's default)
    pub hermes_home: Option<PathBuf>,
}

/// The production sync: the model diffed against / written into a live profile behind a restore point, with
/// the ownership rules in `sync` (Hermes-side edits are conflicts, never overwritten; jobs created paused).
impl Target for HermesProfile {
    type Opts = Opts;

    fn plan(&self, m: &BotModel, src: &Source, dest: &Path, o: &Opts) -> Res<Plan> {
        let p = sync::plan(&sync::Ctx::new(m, src, dest, o))?;
        Ok(Plan {
            items: p.items,
            synced_digest: p.lock.map(|l| l.bundle_digest),
        })
    }

    fn apply(
        &self,
        m: &BotModel,
        src: &Source,
        dest: &Path,
        o: &Opts,
        adopt: bool,
        dry: bool,
    ) -> Res<Applied> {
        sync::apply(&sync::Ctx::new(m, src, dest, o), adopt, dry)
    }
}

impl Adapter for HermesProfile {
    fn id(&self) -> &'static str {
        ID
    }

    fn read(&self, src: &Path) -> Res<BotModel> {
        if !src.is_dir() {
            return err(format!("profile not found: {}", src.display()));
        }
        let text = |rel: &str| std::fs::read_to_string(src.join(rel)).unwrap_or_default();
        let mut memory = vec![];
        for (file, tier) in [
            ("memories/MEMORY.md", "note"),
            ("memories/USER.md", "profile"),
        ] {
            for e in sync::entries(&text(file)) {
                memory.push(Fact {
                    text: e,
                    tier: Some(tier.into()),
                    scope: None,
                    source: None,
                    date: None,
                    origin_side: None,
                });
            }
        }
        let mut skills = vec![];
        let sroot = src.join("skills");
        for rel in files_sorted(&sroot) {
            let r = rel_str(&rel);
            let Some(d) = r.strip_suffix("SKILL.md") else {
                continue;
            };
            let d = d.trim_end_matches('/');
            if d.split('/').any(|p| {
                p.starts_with('.') || ["references", "templates", "assets", "scripts"].contains(&p)
            }) {
                continue;
            }
            let name = d.rsplit('/').next().unwrap_or(d);
            skills.push(Skill::from_bytes(name, &read_tree(&sroot.join(d))?));
        }
        let (credentials, manual) = read_manual(src);
        Ok(BotModel {
            schema: format!("{SCHEMA_FAMILY}/{SCHEMA_MAJOR}"),
            persona: Persona {
                name: src
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                instructions: text("SOUL.md"),
            },
            memory,
            memory_text: None,
            skills,
            schedules: read_schedules(src),
            connectors: read_connectors(src)?,
            credentials,
            history: None,
            manual,
            extras: match unmapped(src)? {
                Some(x) => [(ID.to_string(), x)].into_iter().collect(),
                None => Default::default(),
            },
        })
    }

    /// The plain model mapping into a fresh profile directory: SOUL.md; memories/MEMORY.md and USER.md
    /// (§-separated; user-scope or `profile`-tier facts go to USER.md); skills/<name>/...; the bot's schedules as
    /// cron/jobs.json entries (always paused); MCP connectors under config.yaml `mcp_servers` (on top of the
    /// profile config kept in `extras`); and botport-manual.md for credential names and everything else that
    /// needs a human. Live profiles go through `Target::plan`/`apply` instead (cron via the hermes CLI).
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>> {
        let join = |user: bool| {
            m.memory
                .iter()
                .filter(|f| sync::is_user_fact(f) == user)
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>()
                .join("\n§\n")
        };
        let mut out = vec![
            (
                "SOUL.md".to_string(),
                m.persona.instructions.clone().into_bytes(),
            ),
            ("memories/MEMORY.md".into(), join(false).into_bytes()),
        ];
        let user = join(true);
        if !user.is_empty() {
            out.push(("memories/USER.md".into(), user.into_bytes()));
        }
        for s in &m.skills {
            for (rel, body) in s.bytes() {
                out.push((format!("skills/{}/{rel}", s.name), body));
            }
        }
        let kept = m.extras.get(ID).and_then(|x| x.get("config")).cloned();
        // Each model connector goes under a kept `mcp_servers` key with its own name (skipped when that entry
        // already says the same, as `read_connectors` reads it), else under its slug, else (the slug is a
        // different kept server) under the first free `<slug>-N`: a kept server is never merged into another.
        let servers = kept
            .as_ref()
            .and_then(|c| c.get("mcp_servers")?.as_object().cloned())
            .unwrap_or_default();
        let mut m = m.clone();
        let connectors = std::mem::take(&mut m.connectors);
        let (jobs, mut mcp, mut manual) = sync::from_described(&m, &[], &[], &[]);
        let mut taken: Vec<String> = servers.keys().cloned().collect();
        for c in connectors {
            let key = match servers.get(&c.name) {
                Some(v) => {
                    let tools: Vec<&str> = v["tools"]["include"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|t| t.as_str())
                        .collect();
                    if v["url"].as_str() == c.url.as_deref() && tools == c.tools {
                        continue;
                    }
                    Some(c.name.clone())
                }
                None => None,
            };
            let one = BotModel {
                connectors: vec![c.clone()],
                schedules: vec![],
                credentials: vec![],
                manual: vec![],
                ..m.clone()
            };
            let (_, entries, items) = sync::from_described(&one, &[], &[], &[]);
            manual.extend(items);
            for (id, entry) in entries {
                let id = key.clone().unwrap_or_else(|| {
                    let free = |k: &String| !taken.contains(k);
                    std::iter::once(id.clone())
                        .chain((2..).map(|n| format!("{id}-{n}")))
                        .find(free)
                        .unwrap()
                });
                taken.push(id.clone());
                mcp.push((id, entry));
            }
        }
        manual.sort();
        manual.dedup_by(|a, b| a.0 == b.0);
        if !jobs.is_empty() {
            // names stay as the schedules have them: the `<Bot>: ` prefix is for jobs in a shared live profile
            let prefix = format!("{}: ", sync::bot_name(&m));
            let jobs: Vec<serde_json::Value> = jobs
                .iter()
                .map(|j| {
                    let name = j.name.strip_prefix(&prefix).unwrap_or(&j.name);
                    json!({"name": name, "prompt": j.prompt, "deliver": j.deliver, "enabled": false,
                    "schedule": {"kind": "cron", "expr": j.schedule}, "paused_reason": j.paused_reason})
                })
                .collect();
            out.push((
                "cron/jobs.json".into(),
                json_sorted(&json!({ "jobs": jobs })).into_bytes(),
            ));
        }
        if !mcp.is_empty() || kept.is_some() {
            let mut cfg: Y = match kept {
                Some(v) => serde_yaml::to_value(v).map_err(|e| e.to_string())?,
                None => Y::Mapping(Default::default()),
            };
            for (id, entry) in mcp {
                // the model's url and tools replace a kept entry's (other kept fields, e.g. headers, stay)
                if let Some(Y::Mapping(e)) = cfg.get_mut("mcp_servers").and_then(|s| s.get_mut(&id))
                {
                    e.remove("url");
                    if let Some(Y::Mapping(t)) = e.get_mut("tools") {
                        t.remove("include");
                    }
                }
                let mut ls = vec![];
                sync::leaves(&entry, &mut vec!["mcp_servers".into(), id], &mut ls);
                for (path, v) in ls {
                    sync::set_path(&mut cfg, &path, v);
                }
            }
            out.push((
                "config.yaml".into(),
                crate::core::yaml11::emit_checked(&cfg)?.into_bytes(),
            ));
        }
        if !manual.is_empty() {
            // items a human ticked off stay ticked
            let mut text = sync::manual_text(&[]);
            for (n, todo) in &manual {
                let done = m
                    .manual
                    .iter()
                    .any(|x| x.done && x.item == *n && x.todo == *todo);
                text += &format!("- [{}] {n}: {todo}\n", if done { "x" } else { " " });
            }
            out.push((sync::MANUAL.into(), text.into_bytes()));
        }
        for (rel, b) in &out {
            write_atomic(&dest.join(rel), b, None)?;
        }
        Ok(out.into_iter().map(|(r, _)| r).collect())
    }
}

/// cron/jobs.json -> schedules (Hermes runs cron in the host's timezone: `local`).
fn read_schedules(src: &Path) -> Vec<Schedule> {
    sync::live_jobs(src)
        .iter()
        .map(|j| {
            let expr = sync::cron_expr(j);
            Schedule {
                name: j["name"].as_str().unwrap_or("").to_string(),
                kind: if expr.is_empty() {
                    ScheduleKind::Event
                } else {
                    ScheduleKind::Scheduled
                },
                cron: (!expr.is_empty()).then_some(expr),
                tz: "local".into(),
                action: j["prompt"].as_str().unwrap_or("").to_string(),
                enabled: j["enabled"] != json!(false),
            }
        })
        .collect()
}

/// config.yaml `mcp_servers` -> connectors (url and `tools.include`; header and auth values are never read).
fn read_connectors(src: &Path) -> Res<Vec<Connector>> {
    let cfg = src.join("config.yaml");
    if !cfg.is_file() {
        return Ok(vec![]);
    }
    let y: Y = serde_yaml::from_str(&crate::core::yaml11::read(&read_str(&cfg)?))
        .map_err(|e| format!("{}: {e}", cfg.display()))?;
    let servers = y.get("mcp_servers").and_then(Y::as_mapping);
    Ok(servers
        .into_iter()
        .flatten()
        .map(|(k, v)| {
            let url = v.get("url").and_then(Y::as_str).map(String::from);
            let http = url
                .as_deref()
                .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"));
            Connector {
                name: k.as_str().unwrap_or("").to_string(),
                kind: if http { "mcp-http" } else { "mcp-stdio" }.into(),
                url,
                tools: v
                    .get("tools")
                    .and_then(|t| t.get("include"))
                    .and_then(Y::as_sequence)
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
            }
        })
        .collect())
}

/// botport-manual.md -> credential names (`credential NAME: ...` lines) and the other open items.
fn read_manual(src: &Path) -> (Vec<CredentialRef>, Vec<ManualItem>) {
    let text = std::fs::read_to_string(src.join(sync::MANUAL)).unwrap_or_default();
    let (mut creds, mut manual) = (vec![], vec![]);
    for l in text.lines() {
        let Some(rest) = ["- [ ] ", "- [x] ", "- [X] "]
            .iter()
            .find_map(|p| l.strip_prefix(p))
        else {
            continue;
        };
        let Some((item, todo)) = split_manual(rest) else {
            continue;
        };
        match item.strip_prefix("credential ") {
            Some(n)
                if !n.starts_with('(') && crate::core::secrets::credential_name(n).is_none() =>
            {
                creds.push(CredentialRef::new(n));
                // a ticked credential line stays ticked: `write` finds its tick among the manual items
                if !l.starts_with("- [ ]") {
                    manual.push(ManualItem {
                        done: true,
                        ..ManualItem::new(item, todo)
                    });
                }
            }
            _ => manual.push(ManualItem {
                done: !l.starts_with("- [ ]"),
                ..ManualItem::new(item, todo)
            }),
        }
    }
    (creds, manual)
}

/// `<item>: <todo>` split at the `": "` before a todo botport writes (names and todos may hold `": "` too), else
/// at the first one.
fn split_manual(rest: &str) -> Option<(&str, &str)> {
    const TODOS: [&str; 5] = [
        "not imported: ",
        "event-triggered on the Grok side",
        "set it on the Hermes side by hand",
        "runs on another machine",
        "app without an MCP url",
    ];
    let known = |t: &str| {
        TODOS.iter().any(|p| t.starts_with(p))
            || (t.starts_with("no ")
                && t.contains(" in the description: create the Hermes cron job"))
            || t.split_once(' ').is_some_and(|(k, r)| {
                !k.contains(':') && r.starts_with("connector: enable the matching")
            })
    };
    rest.match_indices(": ")
        .map(|(i, _)| (&rest[..i], &rest[i + 2..]))
        .find(|(_, t)| known(t))
        .or_else(|| rest.split_once(": "))
}

/// Profile data the model has no field for: config.yaml (secret-looking keys dropped) and the cron jobs.
/// `.env`, `auth.json` and other credential stores are never read.
fn unmapped(src: &Path) -> Res<Option<serde_json::Value>> {
    fn scrub(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.retain(|k, _| {
                    let k = k.to_lowercase();
                    ![
                        "key",
                        "token",
                        "secret",
                        "password",
                        "cookie",
                        "authorization",
                        "auth",
                        "credential",
                        "bearer",
                        "headers",
                    ]
                    .iter()
                    .any(|s| k.contains(s))
                });
                m.values_mut().for_each(scrub);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    let mut x = serde_json::Map::new();
    let cfg = src.join("config.yaml");
    if cfg.is_file() {
        let y: serde_yaml::Value =
            serde_yaml::from_str(&crate::core::yaml11::read(&read_str(&cfg)?))
                .map_err(|e| format!("{}: {e}", cfg.display()))?;
        let mut j = serde_json::to_value(y).map_err(|e| format!("config.yaml: {e}"))?;
        scrub(&mut j);
        x.insert("config".into(), j);
    }
    let jobs = sync::live_jobs(src);
    if !jobs.is_empty() {
        let mut j = serde_json::Value::Array(jobs);
        scrub(&mut j);
        x.insert("cron_jobs".into(), j);
    }
    Ok((!x.is_empty()).then(|| x.into()))
}

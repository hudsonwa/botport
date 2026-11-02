//! Hermes adapter: port spec, plan/apply/sync (`sync`), two-way export/drift (`twoway`), and the `Adapter` impl.
pub mod spec;
pub mod sync;
pub mod twoway;

use crate::core::adapter::Adapter;
use crate::core::model::{BotModel, Fact, Persona, Skill, SCHEMA_FAMILY, SCHEMA_MAJOR};
use crate::core::util::*;
use std::path::Path;

pub const ID: &str = "hermes";

/// A Hermes profile directory (SOUL.md, memories/, skills/).
pub struct HermesProfile;

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
            let (files, _) = super::grok_bot::split_files(&read_tree(&sroot.join(d))?);
            skills.push(Skill {
                name: d.rsplit('/').next().unwrap_or(d).to_string(),
                description: None,
                files,
            });
        }
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
            skills,
            schedules: vec![],
            connectors: vec![],
            credentials: vec![],
            history: None,
            extras: match unmapped(src)? {
                Some(x) => [(ID.to_string(), x)].into_iter().collect(),
                None => Default::default(),
            },
        })
    }

    /// Writes SOUL.md, memories/MEMORY.md and USER.md (§-separated; user-scope or `profile`-tier facts go to USER.md) and skills/<name>/... into a profile directory.
    /// Production syncs go through `sync::plan`/`sync::apply` with a port spec; this is the plain model mapping.
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>> {
        let is_user =
            |f: &&Fact| f.scope.as_deref() == Some("user") || f.tier.as_deref() == Some("profile");
        let join = |user: bool| {
            m.memory
                .iter()
                .filter(|f| is_user(f) == user)
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>()
                .join("\n§\n")
        };
        let mut out = vec![
            ("SOUL.md".to_string(), m.persona.instructions.clone()),
            ("memories/MEMORY.md".into(), join(false)),
        ];
        let user = join(true);
        if !user.is_empty() {
            out.push(("memories/USER.md".into(), user));
        }
        for s in &m.skills {
            for (rel, body) in &s.files {
                out.push((format!("skills/{}/{rel}", s.name), body.clone()));
            }
        }
        for (rel, b) in &out {
            write_atomic(&dest.join(rel), b.as_bytes(), None)?;
        }
        Ok(out.into_iter().map(|(r, _)| r).collect())
    }
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

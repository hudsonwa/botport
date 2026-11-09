//! `botport twin demo`: an invented pair of bots (Acme Helper, Recipe Planner) in a fake Grok app folder inside the
//! twin state folder, so the whole twin flow runs with no Grok account. `--step` scripts the demo's beats on that fake
//! folder only (never on a real Grok app folder). Also: the line diff `twin conflicts` shows.
use crate::adapters::grok_bot::cache::{self, CacheBot};
use crate::core::util::{write_private, Args};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The `--from-export` value meaning "the demo's own exports, one per bot".
pub const EXPORTS_MARK: &str = "@demo";
/// The fake Grok app folder, relative to the state folder.
pub const APP_DIR: &str = "demo-grok-app";
const EXPORTS: &str = "demo/exports";

type R<T> = Result<T, super::twin::Fail>;

fn usage(m: impl Into<String>) -> super::twin::Fail {
    super::twin::Fail::usage(m)
}

fn io(m: String) -> super::twin::Fail {
    super::twin::Fail::internal(m)
}

struct DemoBot {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    routines: &'static [&'static str],
    instructions: &'static str,
    skills: &'static [(&'static str, &'static str, &'static str)],
}

const BOTS: &[DemoBot] = &[
    DemoBot {
        id: "b-acme-0001",
        name: "Acme Helper",
        description: "Answers questions about Acme widgets.",
        routines: &["Morning summary"],
        instructions: "You are Acme Helper, the help desk for Acme widgets.\nAnswer briefly and politely.\nWhen you do not know, say so and offer to pass the question on.\n",
        skills: &[(
            "greet-visitors",
            "Greet a new visitor",
            "Welcome the visitor by name, say what Acme Helper can do, and ask how to help.\n",
        )],
    },
    DemoBot {
        id: "b-recipe-0002",
        name: "Recipe Planner",
        description: "Plans a week of meals from what is in the fridge.",
        routines: &[],
        instructions: "You are Recipe Planner.\nSuggest simple meals for the week from the ingredients the user lists.\n",
        skills: &[(
            "shopping-list",
            "Make a shopping list",
            "Group the missing ingredients by aisle and list them once each.\n",
        )],
    },
];

fn cache_bots(rename: bool, secret: bool) -> Vec<CacheBot> {
    BOTS.iter()
        .map(|b| {
            let mut c = CacheBot {
                id: b.id.into(),
                name: b.name.into(),
                description: b.description.into(),
                routines: b.routines.iter().map(|s| s.to_string()).collect(),
            };
            if b.id == "b-acme-0001" && rename {
                c.name = "Acme Support Desk".into();
                c.description = "Answers questions about Acme widgets and spare parts.".into();
            }
            if b.id == "b-acme-0001" && secret {
                // an obviously fake key, split here so secret scanners of this source do not flag it
                c.description = format!(
                    "{} Test key: {}{}",
                    c.description, "sk-", "demo-0000NOTAREALKEY0000"
                );
            }
            c
        })
        .collect()
}

fn export_doc(b: &DemoBot, skill_text: Option<&str>) -> String {
    let skills: Vec<_> = b
        .skills
        .iter()
        .map(|(n, d, i)| json!({"name": n, "description": d, "instructions": skill_text.unwrap_or(i)}))
        .collect();
    json!({"format": "grok-bot-export", "version": 1, "bots": [{"name": b.name, "title": "", "instructions": b.instructions,
           "skills": skills, "routines": [], "apps": []}]})
    .to_string()
}

fn slug(name: &str) -> String {
    name.to_lowercase().replace(' ', "-")
}

/// The exports `--from-export @demo` stands for: bot name -> file.
pub fn exports(a: &Args, state: &Path) -> R<BTreeMap<String, PathBuf>> {
    if a.get("from-export") != Some(EXPORTS_MARK) {
        return Ok(BTreeMap::new());
    }
    let mut m = BTreeMap::new();
    for b in BOTS {
        let p = state.join(EXPORTS).join(format!("{}.json", slug(b.name)));
        if p.is_file() {
            m.insert(b.name.to_string(), p.clone());
            // the renamed bot keeps its export
            if b.id == "b-acme-0001" {
                m.insert("Acme Support Desk".into(), p);
            }
        }
    }
    Ok(m)
}

fn demo_mode(state: &Path) -> bool {
    fs::read_to_string(state.join("twin.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| v["grok_app_dir"] == APP_DIR)
}

/// The demo twin of a bot: (pair id, profile), from the index.
fn demo_pair(state: &Path, grok_id: &str) -> R<(String, String)> {
    let v: serde_json::Value = fs::read_to_string(state.join("twin.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or_else(|| usage("no demo here: run botport twin demo first"))?;
    v["pairs"]
        .as_object()
        .into_iter()
        .flatten()
        .find(|(_, r)| r["grok_id"] == grok_id)
        .and_then(|(p, r)| Some((p.clone(), r["profile"].as_str()?.to_string())))
        .filter(|(p, pr)| super::twin::safe_component(p) && super::twin::safe_component(pr))
        .ok_or_else(|| {
            usage("the demo twin is gone: run botport twin demo again with a fresh --state-dir")
        })
}

/// Set up the demo (no `--step`: returns None, and the caller twins every demo bot), or run one step on the fake app
/// folder (returns the step; the caller then syncs).
pub fn demo(a: &Args, hermes_home: &Path, state: &Path) -> R<Option<String>> {
    let app = state.join(APP_DIR);
    let Some(step) = a.get("step") else {
        // the demo writes invented profiles: never into a Hermes home that already has profiles of its own
        let has_profiles =
            fs::read_dir(hermes_home.join("profiles")).is_ok_and(|mut d| d.next().is_some());
        if has_profiles && !demo_mode(state) {
            return Err(usage(
                "this Hermes home already has profiles: give the demo its own --hermes-home and --state-dir",
            ));
        }
        crate::core::util::ensure_private_dir(&hermes_home.join("profiles")).map_err(io)?;
        if !demo_mode(state) && state.join("twin.json").exists() {
            return Err(usage(
                "this state folder already holds real twins: give the demo its own --state-dir and --hermes-home",
            ));
        }
        cache::write_fixture(&app, &cache_bots(false, false)).map_err(io)?;
        for b in BOTS {
            write_private(
                state,
                &format!("{EXPORTS}/{}.json", slug(b.name)),
                export_doc(b, None).as_bytes(),
            )
            .map_err(io)?;
        }
        if !state.join("twin.json").exists() {
            let idx = json!({"schema": "botport-twin/1", "grok_app_dir": APP_DIR, "pairs": {}});
            write_private(state, "twin.json", idx.to_string().as_bytes()).map_err(io)?;
        }
        return Ok(None);
    };
    if !demo_mode(state) {
        return Err(usage(
            "no demo here: run botport twin demo first (with its own --state-dir)",
        ));
    }
    match step {
        "rename" => cache::write_fixture(&app, &cache_bots(true, false)).map_err(io)?,
        "secret" => cache::write_fixture(&app, &cache_bots(false, true)).map_err(io)?,
        "reset" => cache::write_fixture(&app, &cache_bots(false, false)).map_err(io)?,
        "conflict" => {
            // the same skill changed on both sides since the last sync; the twin is found through the index (its
            // profile may not be named like the bot)
            let (pair, profile) = demo_pair(state, "b-acme-0001")?;
            let b = &BOTS[0];
            let grok = "Welcome the visitor by name and mention this week's new widget colours.\n";
            let doc = state.join(EXPORTS).join("acme-conflict.json");
            write_private(
                state,
                &format!("{EXPORTS}/acme-conflict.json"),
                export_doc(b, Some(grok)).as_bytes(),
            )
            .map_err(io)?;
            let mut items = crate::adapters::grok_bot::mirror::read_items(&doc).map_err(io)?;
            super::twin::save_source_in(state, &pair, &mut items)?;
            let skill = hermes_home
                .join("profiles")
                .join(&profile)
                .join("skills")
                .join(super::twin::CATEGORY)
                .join("greet-visitors/SKILL.md");
            let t = fs::read_to_string(&skill).map_err(|_| {
                usage("the Acme Helper twin has no greet-visitors skill yet: run botport twin demo first")
            })?;
            let t = format!(
                "{}\nAlways end with: \"Anything else I can help with?\"\n",
                t.trim_end()
            );
            crate::core::util::write_atomic(&skill, t.as_bytes(), Some(0o600)).map_err(io)?;
        }
        s => {
            return Err(usage(format!(
                "unknown demo step {s:?}: rename, secret, conflict or reset"
            )))
        }
    }
    Ok(Some(step.to_string()))
}

/// A unified-style line diff (`-` only in `a`, `+` only in `b`), for display; at most 2000 lines a side, else a note.
pub fn unified_diff(a: &str, b: &str, an: &str, bn: &str) -> String {
    let x: Vec<&str> = a.lines().collect();
    let y: Vec<&str> = b.lines().collect();
    let mut out = format!("--- {an}\n+++ {bn}\n");
    if x.len() > 2000 || y.len() > 2000 {
        out.push_str("@@ the copies are too long to compare line by line @@\n");
        return out;
    }
    // longest common subsequence table
    let (n, m) = (x.len(), y.len());
    let mut l = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if x[i] == y[j] {
                l[i + 1][j + 1] + 1
            } else {
                l[i + 1][j].max(l[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && x[i] == y[j] {
            out.push_str(&format!(" {}\n", x[i]));
            i += 1;
            j += 1;
        } else if j < m && (i == n || l[i][j + 1] >= l[i + 1][j]) {
            out.push_str(&format!("+{}\n", y[j]));
            j += 1;
        } else {
            out.push_str(&format!("-{}\n", x[i]));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_marks_changes() {
        let d = unified_diff("a\nb\nc\n", "a\nc\nd\n", "grok", "hermes");
        assert_eq!(d, "--- grok\n+++ hermes\n a\n-b\n c\n+d\n");
    }

    #[test]
    fn demo_bots_are_invented_and_secret_step_is_caught() {
        let bots = cache_bots(false, true);
        let d = &bots[0].description;
        assert!(crate::core::secrets::find_secret("persona.description", d.as_bytes()).is_some());
        assert!(cache_bots(false, false)
            .iter()
            .all(|b| crate::core::secrets::find_secret(
                "persona.description",
                b.description.as_bytes()
            )
            .is_none()));
    }
}

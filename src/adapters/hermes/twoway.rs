//! Hermes -> Grok direction: `export-hermes` builds a bundle of what the Hermes bot learned (memory facts, skills it
//! created or edited, cron changes) that the Grok bot loads by following docs/AGENT-IMPORT.md.
//!
//! Merge rules (the repo is the hub; persona/SOUL/instructions are one-way from the repo and never exported):
//! - memory: union by normalized fact. Each exported fact carries `source: "hermes"`. Echo prevention: a fact the
//!   Grok side already holds (any source, any spelling that normalizes the same) is never exported back; USER.md seed
//!   entries come from the repo and are not exported either.
//! - skills: a skill the Hermes bot created or patched (per Hermes's own skills/.usage.json provenance) or an owned
//!   skill edited on the Hermes side since the last sync. If the Grok side changed the same skill since that sync, it
//!   is a CONFLICT, reported and not exported. A skill whose identical copy is already in the Grok export is skipped.
//! - cron: jobs that differ from the repo's cron spec become requested actions (Grok cannot set schedules by file).
use super::sync::{self, Ctx, Item, St};
use crate::adapters::grok_bot::bundle::{secret_content, TOOL, VERSION};
use crate::core::util::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Fact identity for the union: lower case, whitespace collapsed, list bullets and trailing punctuation dropped.
pub fn norm(f: &str) -> String {
    let t = f
        .trim()
        .trim_start_matches(['-', '*', '•', '§', ' '])
        .trim();
    // the import brief tells the Grok bot to tag imported facts "(from Hermes) …": the same fact, not a new one
    let t = match t.get(..13) {
        Some(p) if p.eq_ignore_ascii_case("(from hermes)") => t[13..].trim(),
        _ => t,
    };
    let t = t.trim_end_matches(['.', '!', ';', ',', ' ']);
    t.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Union of `incoming` into `have` by normalized fact; returns the facts that were new (input order, deduplicated).
pub fn union_new(have: &BTreeSet<String>, incoming: &[String]) -> Vec<String> {
    let mut seen = have.clone();
    let mut out = vec![];
    for f in incoming {
        let n = norm(f);
        if !n.is_empty() && seen.insert(n) {
            out.push(f.trim().to_string());
        }
    }
    out
}

pub struct Export {
    /// sent skills present in the current Grok export (imported by the Grok bot)
    pub seen: BTreeSet<String>,
    /// skills whose Hermes and Grok copies are equal again (a conflict merged by hand): record re-baselined
    pub resolved: BTreeMap<String, String>,
    pub facts: Vec<Value>,
    pub skills: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    pub cron: Vec<Value>,
    pub items: Vec<Item>,
}

fn skill_dirs(root: &Path) -> BTreeMap<String, std::path::PathBuf> {
    // skill dir (path relative to root) for every SKILL.md, not inside another skill's support folder
    let mut out = BTreeMap::new();
    for rel in files_sorted(root) {
        let r = rel_str(&rel);
        if let Some(d) = r.strip_suffix("/SKILL.md") {
            if !d.split('/').any(|p| {
                p.starts_with('.') || ["references", "templates", "assets", "scripts"].contains(&p)
            }) {
                out.insert(d.to_string(), root.join(d));
            }
        }
    }
    out
}

/// What the Hermes profile holds that the Grok side does not (read-only).
pub fn collect(c: &Ctx) -> Res<Export> {
    let w = sync::want(c)?;
    let lock = sync::read_lock(&c.dest).unwrap_or_default();
    let mut items = vec![];
    // --- memory
    let grok: BTreeSet<String> = c
        .model
        .memory
        .iter()
        .map(|f| norm(&f.text))
        .filter(|f| !f.is_empty())
        .collect();
    let seed: BTreeSet<String> = w
        .user_seed
        .as_deref()
        .map(sync::entries)
        .unwrap_or_default()
        .iter()
        .map(|e| norm(e))
        .collect();
    let mut facts = vec![];
    let mut have = grok.clone();
    have.extend(seed.iter().cloned());
    for (file, tier) in [
        ("memories/MEMORY.md", "note"),
        ("memories/USER.md", "profile"),
    ] {
        let text = fs::read_to_string(c.dest.join(file)).unwrap_or_default();
        let new = union_new(&have, &sync::entries(&text));
        for f in new {
            have.insert(norm(&f));
            if secret_content(f.as_bytes()) {
                items.push(sync::item(
                    "memory",
                    file,
                    St::Skip,
                    "a fact with credential-like text was left out",
                ));
                continue;
            }
            facts.push(
                json!({"date": "", "tier": tier, "scope": "hermes", "source": "hermes", "fact": f}),
            );
        }
    }
    facts.sort_by_key(|r| {
        (
            r["tier"].as_str().unwrap_or("").to_string(),
            norm(r["fact"].as_str().unwrap_or("")),
        )
    });
    if !facts.is_empty() {
        items.push(sync::item(
            "memory",
            "facts",
            St::Create,
            format!("{} fact(s) new to the Grok side", facts.len()),
        ));
    }
    // --- skills
    let mut skills = BTreeMap::new();
    let mut resolved = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut add = |name: String,
                   files: BTreeMap<String, Vec<u8>>,
                   why: &str,
                   items: &mut Vec<Item>| {
        if files.values().any(|b| secret_content(b)) {
            items.push(sync::item(
                "skills",
                &name,
                St::Skip,
                "credential-like text: not exported",
            ));
        } else if c.skills.get(&name).map(tree_digest_files) == Some(tree_digest_files(&files)) {
            // identical copy already on the Grok side: nothing to send
        } else if let std::collections::btree_map::Entry::Vacant(v) = skills.entry(name.clone()) {
            items.push(sync::item("skills", &name, St::Create, why));
            v.insert(files);
        } else {
            items.push(sync::item(
                "skills",
                &name,
                St::Conflict,
                "two Hermes skills share this name; rename one",
            ));
        }
    };
    // Outside the owned category, only what the bot itself made or changed counts. Hermes records that in
    // skills/.usage.json (skill_manage writes `created_by: "agent"` and patch counts); skills copied in by Hermes
    // updates, the hub or a profile clone carry no such mark and are never exported.
    let root = c.dest.join("skills");
    let usage: Value = fs::read(root.join(".usage.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for (rel, dir) in skill_dirs(&root) {
        if rel.split('/').next() == Some(w.cat.as_str()) {
            continue;
        }
        let dirname = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        if c.source.excluded.contains(&dirname) || lock.grok_excluded.contains(&dirname) {
            continue; // a Grok skill this profile does not take is never Hermes-made nor sent back
        }
        let fm = sync::skill_name(&dir.join("SKILL.md"));
        let rec = usage
            .get(&fm)
            .or_else(|| usage.get(&dirname))
            .cloned()
            .unwrap_or_default();
        // skill_manage stamps "agent" (autonomous) or "learn" (taught in chat); Hermes-installed skills have none
        let created = rec["created_by"] == json!("agent")
            || rec["created_by"] == json!("learn")
            || rec["agent_created"] == json!(true);
        let patched = rec["patch_count"].as_u64().unwrap_or(0) > 0;
        if !created && !patched {
            continue;
        }
        let files = read_tree(&dir)?;
        // Sent before: if the Grok copy changed since then, a further Hermes change is a both-sides conflict;
        // if only the Grok side changed, there is nothing to send (the Grok -> Hermes side reports it).
        let live_d = tree_digest_files(&files);
        let grok_d = c.skills.get(&dirname).map(tree_digest_files);
        if let Some(sent) = lock.h2g_skills.get(&dirname) {
            // Versions the Grok side may hold without having changed anything: any version ever sent (an older one
            // while a newer send is still unread) or agreed.
            let known = |g: &String| {
                g == sent
                    || lock
                        .h2g_history
                        .get(&dirname)
                        .is_some_and(|h| h.contains(g))
            };
            if grok_d.is_some() {
                seen.insert(dirname.clone());
            }
            match &grok_d {
                Some(g) if *g == live_d => {
                    // both copies equal (imported, or a conflict merged by hand)
                    if *sent != live_d || !known(&live_d) {
                        resolved.insert(dirname.clone(), live_d.clone());
                    }
                    continue;
                }
                None if lock.h2g_seen.contains(&dirname) && live_d == *sent => {
                    items.push(sync::item(
                        "skills",
                        &dirname,
                        St::InSync,
                        "removed on the Grok side; not sent again",
                    ));
                    continue;
                }
                None => {} // not imported yet: stays pending (and in the bundle)
                Some(g) if known(g) => {} // Grok unchanged; the Hermes copy differs from it -> (re)send below
                Some(_) if live_d == *sent => continue, // only the Grok side changed: the Grok -> Hermes side reports it
                Some(_) => {
                    items.push(sync::item("skills", &dirname, St::Conflict,
                        "changed on both sides since it was sent to Grok; not exported, not overwritten — merge by hand"));
                    continue;
                }
            }
        }
        add(
            dirname,
            files,
            if created {
                "created by the Hermes bot"
            } else {
                "edited by the Hermes bot"
            },
            &mut items,
        );
    }
    // owned skills edited on the Hermes side since the last sync
    let mem = c
        .spec
        .memory_skill
        .as_ref()
        .map(|m| m.name.clone())
        .unwrap_or(format!("{}-memory", w.cat));
    for (u, want_files) in &w.units {
        let name = u.rsplit('/').next().unwrap_or(u).to_string();
        if name == mem || name == "DESCRIPTION.md" {
            continue;
        }
        let Some(live) = sync::live_unit(&c.dest, u) else {
            continue;
        };
        let ld = tree_digest_files(&live);
        let rec = lock.skills.get(u);
        if rec.is_none() || rec == Some(&ld) || ld == tree_digest_files(want_files) {
            continue; // never synced, untouched since, or equal to the export
        }
        if Some(&tree_digest_files(want_files)) != rec {
            items.push(sync::item("skills", &name, St::Conflict,
                "changed on both sides since the last sync; not exported, not overwritten — merge by hand"));
            continue;
        }
        add(name, live, "edited on the Hermes side", &mut items);
    }
    // --- cron
    let spec: BTreeMap<String, &sync::CronJob> =
        w.cron.iter().map(|j| (j.name.clone(), j)).collect();
    let mut cron = vec![];
    for j in sync::live_jobs(&c.dest) {
        let name = j["name"].as_str().unwrap_or("").to_string();
        if w.model_cron.iter().any(|m| m.name == name) {
            continue; // made from the Grok bot's own routine: it is already there
        }
        let row = json!({"name": name, "schedule": sync::cron_expr(&j), "deliver": j["deliver"],
                         "prompt": j["prompt"], "enabled": j["enabled"] != json!(false)});
        let same = spec
            .get(&name)
            .map(|s| {
                s.schedule == sync::cron_expr(&j)
                    && j["prompt"].as_str() == Some(&s.prompt)
                    && j["deliver"].as_str() == Some(&s.deliver)
                    && j["enabled"] != json!(false)
            })
            .unwrap_or(false);
        if !same && !name.is_empty() {
            if secret_content(row.to_string().as_bytes()) {
                items.push(sync::item(
                    "cron",
                    &name,
                    St::Skip,
                    "credential-like text: not exported",
                ));
                continue;
            }
            // Grok cannot export schedules back, so a request counts as pending only until it was sent once
            // (unchanged); the bundle still carries it, so a re-written bundle never drops an unread request.
            if lock.h2g_cron.get(&name) == Some(&sha256_hex(row.to_string().as_bytes())) {
                items.push(sync::item("cron", &name, St::InSync, "requested earlier"));
            } else {
                items.push(sync::item(
                    "cron",
                    &name,
                    St::Create,
                    "schedule to request from the Grok bot",
                ));
            }
            cron.push(row);
        }
    }
    cron.sort_by_key(|r| r["name"].as_str().unwrap_or("").to_string());
    Ok(Export {
        seen,
        resolved,
        facts,
        skills,
        cron,
        items,
    })
}

fn requests_md(e: &Export) -> String {
    let mut s = String::from(
        "# Requested actions for the Grok bot\n\nThese cannot be set by file on the Grok side. \
Carry each one out with your own tools (automations/schedules), then report what you did.\n\n",
    );
    if e.cron.is_empty() {
        return s + "(none)\n";
    }
    for j in &e.cron {
        s += &format!(
            "- Schedule `{}`: cron `{}` ({}), deliver to {}. Task: {}\n",
            j["name"].as_str().unwrap_or(""),
            j["schedule"].as_str().unwrap_or(""),
            if j["enabled"] == json!(true) {
                "enabled"
            } else {
                "paused"
            },
            j["deliver"].as_str().unwrap_or("?"),
            j["prompt"].as_str().unwrap_or("").replace('\n', " ")
        );
    }
    s
}

/// Write the Hermes-side bundle (same manifest and determinism rules as `export`). Returns its file map.
pub fn write(e: &Export, c: &Ctx, out: &Path) -> Res<BTreeMap<String, String>> {
    crate::adapters::grok_bot::bundle::clear_out(out)?;
    let mut files = BTreeMap::new();
    let mut put = |rel: String, data: &[u8]| -> Res<()> {
        write_atomic(&out.join(&rel), data, Some(0o644))?;
        files.insert(rel, sha256_hex(data));
        Ok(())
    };
    let jl: String = e.facts.iter().map(jsonl_line).collect();
    put("memory/memory.jsonl".into(), jl.as_bytes())?;
    for (n, f) in &e.skills {
        for (rel, b) in f {
            put(format!("normalized/skills/{n}/{rel}"), b)?;
        }
    }
    put("cron/requests.json".into(), json_sorted(&e.cron).as_bytes())?;
    put("REQUESTS.md".into(), requests_md(e).as_bytes())?;
    let manifest = json!({"tool": TOOL, "version": VERSION, "origin": "hermes", "profile": c.profile,
                          "agent_id": c.source.id, "files": files, "digest": digest_pairs(&files)});
    write_atomic(
        &out.join("manifest.json"),
        json_sorted(&manifest).as_bytes(),
        Some(0o644),
    )?;
    Ok(files)
}

/// Record exported skills (not installed back when Grok re-exports them) and cron requests (asked once).
pub fn record(e: &Export, c: &Ctx) -> Res<()> {
    let mut lock = sync::read_lock(&c.dest).unwrap_or_default();
    if e.skills.is_empty()
        && e.cron.is_empty()
        && e.resolved.is_empty()
        && e.seen.is_subset(&lock.h2g_seen)
    {
        return Ok(());
    }
    for (n, d) in &e.resolved {
        lock.h2g_skills.insert(n.clone(), d.clone());
        lock.h2g_history
            .entry(n.clone())
            .or_default()
            .insert(d.clone());
    }
    lock.h2g_seen.extend(e.seen.iter().cloned());
    for (n, f) in &e.skills {
        lock.h2g_skills.insert(n.clone(), tree_digest_files(f));
        lock.h2g_history
            .entry(n.clone())
            .or_default()
            .insert(tree_digest_files(f));
    }
    for j in &e.cron {
        lock.h2g_cron.insert(
            j["name"].as_str().unwrap_or("").to_string(),
            sha256_hex(j.to_string().as_bytes()),
        );
    }
    write_atomic(
        &c.dest.join(sync::LOCK),
        json_sorted(&lock).as_bytes(),
        Some(0o644),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny deterministic generator (no extra crates).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn fact(&mut self) -> String {
            let words = [
                "Demo", "port", "12345", "runs", "hourly", "at", ":20", "Robin", "prefers", "CSV",
            ];
            let n = 1 + self.next() % 5;
            let mut s: String = (0..n)
                .map(|_| words[(self.next() % words.len() as u64) as usize])
                .collect::<Vec<_>>()
                .join(" ");
            match self.next() % 4 {
                0 => s = format!("- {s}."),
                1 => s = s.to_uppercase(),
                2 => s = format!("  {}  ", s.replace(' ', "   ")),
                _ => {}
            }
            s
        }
    }

    #[test]
    fn norm_is_idempotent_and_spelling_blind() {
        let mut g = Lcg(7);
        for _ in 0..2000 {
            let f = g.fact();
            assert_eq!(norm(&norm(&f)), norm(&f));
        }
        assert_eq!(norm("- Demo  runs hourly."), norm("demo runs HOURLY"));
        assert_eq!(
            norm("(from Hermes) The app restarts on Sundays."),
            norm("the app restarts on sundays")
        );
    }

    #[test]
    fn union_never_duplicates_and_is_stable_on_repeat() {
        let mut g = Lcg(42);
        for _ in 0..300 {
            let have: BTreeSet<String> = (0..g.next() % 6).map(|_| norm(&g.fact())).collect();
            let inc: Vec<String> = (0..g.next() % 8).map(|_| g.fact()).collect();
            let new = union_new(&have, &inc);
            let n: BTreeSet<String> = new.iter().map(|f| norm(f)).collect();
            assert_eq!(n.len(), new.len(), "no duplicates");
            assert!(
                n.is_disjoint(&have),
                "echo: nothing already held is returned"
            );
            // after merging, a second pass returns nothing (convergence / no loop)
            let mut merged = have.clone();
            merged.extend(n);
            assert!(union_new(&merged, &inc).is_empty());
        }
    }
}

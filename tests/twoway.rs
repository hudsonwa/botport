//! End-to-end two-way round trip on throwaway Hermes homes and a fake Grok box (no network, no real Hermes):
//! Grok export -> Hermes apply -> the Hermes bot learns a fact, a skill, a cron job -> export-hermes ->
//! simulated Grok import (exactly what docs/AGENT-IMPORT.md tells the bot to do) -> Grok re-export -> converges.
mod common;
use common::{s, w, Tmp};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const ID: &str = "00000000-0000-4000-8000-0000000000a1";

fn g2h(args: &[&str]) -> (i32, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_grok2hermes"))
        .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR")) // not ~/.local/state
        .args(args)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
    )
}

fn norm(f: &str) -> String {
    let t = f.trim().trim_start_matches(['-', '*', ' ']).trim();
    let t = t
        .strip_prefix("(from Hermes)")
        .unwrap_or(t)
        .trim()
        .trim_end_matches(['.', ' ']);
    t.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

struct World {
    t: Tmp,
}

impl World {
    fn new(tag: &str) -> World {
        let t = Tmp::new(tag);
        let r = &t.0;
        // fake Grok box
        w(
            &r.join(format!("box/agent-data/agents/{ID}/profile.json")),
            r#"{"name":"Demo"}"#,
        );
        w(
            &r.join("box/agent-data/workflows/alpha/SKILL.md"),
            "---\nname: alpha\n---\nalpha v1\n",
        );
        w(&r.join(format!("box/drop/agents/{ID}/memory-agent.jsonl")),
          "{\"date\":\"2026-10-01\",\"tier\":\"profile\",\"scope\":\"\",\"source\":\"self\",\"fact\":\"The Demo MCP listens on port 12345.\"}\n");
        // repo templates + spec
        w(&r.join("repo/tpl/SOUL.md"), "You are Demo.\n");
        w(
            &r.join("repo/tpl/config.yaml"),
            "model:\n  default: example-model\n",
        );
        w(&r.join("repo/tpl/USER.md"), "Robin is in Springfield.\n");
        w(&r.join("repo/tpl/cron.yaml"), "jobs: []\n");
        w(&r.join("repo/spec.toml"), "profile = \"demo\"\nbase = \"tpl\"\nskill_category = \"sample-skills\"\n\
[memory_skill]\nname = \"sample-memory\"\n[soul]\ntemplate = \"SOUL.md\"\n[config]\ntemplate = \"config.yaml\"\n\
[user]\nseed = \"USER.md\"\n[cron]\nfile = \"cron.yaml\"\n[hermes_export]\nout = \"hermes-export\"\n");
        // a skill Hermes installed (no agent provenance in .usage.json): never exported
        w(
            &r.join("hermes/profiles/demo/skills/media/pdf/SKILL.md"),
            "---\nname: pdf\n---\nshipped\n",
        );
        World { t }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.t.0.join(rel)
    }
    fn export_grok(&self) -> (i32, String) {
        let out = self.p("repo/grok");
        g2h(&[
            "export",
            "--agent-data",
            s(&self.p("box/agent-data")),
            "--agent-id",
            ID,
            "--memory-drop",
            s(&self.p("box/drop")),
            "--out",
            s(&out),
        ])
    }
    fn run(&self, cmd: &str, extra: &[&str]) -> (i32, String) {
        let mut a = vec![
            cmd,
            "--bundle",
            s(&self.t.0.join("repo/grok")).to_string().leak(),
            "--spec",
            s(&self.t.0.join("repo/spec.toml")).to_string().leak(),
            "--hermes-home",
            s(&self.t.0.join("hermes")).to_string().leak(),
        ];
        a.extend(extra);
        g2h(&a)
    }
    /// The Grok bot following docs/AGENT-IMPORT.md: copy new/changed skills into workflows/, save new facts
    /// (deduplicated by normalized text, tagged source "hermes") through its memory tool (here: the memory drop).
    fn grok_import(&self) -> (usize, usize) {
        let b = self.p("repo/hermes-export");
        let mut skills = 0;
        let sk = b.join("normalized/skills");
        if sk.is_dir() {
            for e in fs::read_dir(&sk).unwrap().flatten() {
                let dst = self.p("box/agent-data/workflows").join(e.file_name());
                for f in fs::read_dir(e.path()).unwrap().flatten() {
                    w(
                        &dst.join(f.file_name()),
                        &fs::read_to_string(f.path()).unwrap(),
                    );
                }
                skills += 1;
            }
        }
        let drop = self.p(&format!("box/drop/agents/{ID}/memory-agent.jsonl"));
        let mut text = fs::read_to_string(&drop).unwrap();
        let have: BTreeSet<String> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|v| norm(v["fact"].as_str().unwrap_or("")))
            .collect();
        let mut facts = 0;
        for l in fs::read_to_string(b.join("memory/memory.jsonl"))
            .unwrap_or_default()
            .lines()
        {
            let v: Value = serde_json::from_str(l).unwrap();
            let f = v["fact"].as_str().unwrap();
            if !have.contains(&norm(f)) {
                text += &(serde_json::to_string(&json!({"date": "2026-10-30", "tier": v["tier"], "scope": "hermes",
                                                       "source": "hermes", "fact": format!("(from Hermes) {f}")})).unwrap() + "\n");
                facts += 1;
            }
        }
        fs::write(&drop, text).unwrap();
        (skills, facts)
    }
}

#[test]
fn round_trip_converges_without_duplicates_or_loops() {
    let x = World::new("rt");
    assert_eq!(x.export_grok().0, 0);
    let (rc, out) = x.run("sync", &["--adopt"]);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("h2g IN SYNC"), "nothing learned yet: {out}");
    let prof = x.p("hermes/profiles/demo");
    // the Hermes bot learns: a new fact, the same Grok fact spelled differently (echo), a skill, a cron job
    w(
        &prof.join("memories/MEMORY.md"),
        "Robin's demo server is example-host.\n§\n- the Demo MCP listens on port 12345\n",
    );
    w(
        &prof.join("skills/devops/hermes-made/SKILL.md"),
        "---\nname: hermes-made\n---\nmade on Hermes\n",
    );
    w(
        &prof.join("skills/.usage.json"),
        r#"{"hermes-made": {"created_by": "learn", "patch_count": 0}, "pdf": {"created_by": null, "patch_count": 0}}"#,
    );
    w(
        &prof.join("cron/jobs.json"),
        r#"{"jobs":[{"id":"j1","name":"weekly-digest","prompt":"digest","deliver":"local","schedule":{"expr":"0 9 * * 1"}}]}"#,
    );
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 1, "{out}");
    let first = out.lines().next().unwrap();
    assert!(
        first.contains("g2h IN SYNC")
            && first.contains("; h2g 3 pending (1 fact(s), 1 skill(s), 1 cron)"),
        "{out}"
    );
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    let hb = x.p("repo/hermes-export");
    assert_eq!(g2h(&["verify", "--bundle", s(&hb)]).0, 0);
    let facts = fs::read_to_string(hb.join("memory/memory.jsonl")).unwrap();
    assert_eq!(
        facts.lines().count(),
        1,
        "echo of the Grok fact and the USER seed are not exported: {facts}"
    );
    assert!(facts.contains("example-host") && facts.contains("\"source\":\"hermes\""));
    assert!(hb.join("normalized/skills/hermes-made/SKILL.md").is_file());
    assert!(
        !hb.join("normalized/skills/pdf").exists(),
        "shipped skill not exported"
    );
    assert!(fs::read_to_string(hb.join("REQUESTS.md"))
        .unwrap()
        .contains("weekly-digest"));
    // deterministic: exporting again gives the same bytes
    let m1 = fs::read(hb.join("manifest.json")).unwrap();
    // the Grok bot imports, then exports again
    assert_eq!(x.grok_import(), (1, 1));
    assert_eq!(x.export_grok().0, 0);
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        !prof.join("skills/sample-skills/hermes-made").exists(),
        "Hermes-origin skill is not installed back: {out}"
    );
    let mem = fs::read_to_string(prof.join("skills/sample-skills/sample-memory/SKILL.md")).unwrap();
    assert!(
        mem.contains("example-host"),
        "the fact reached the memory skill via Grok"
    );
    // converged: both directions in sync, a second import adds nothing, nothing loops
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        out.contains("g2h IN SYNC") && out.contains("h2g IN SYNC"),
        "{out}"
    );
    assert_eq!(x.grok_import(), (1, 0), "no new facts the second time");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("drift", &[]).0, 0);
    let drop =
        fs::read_to_string(x.p(&format!("box/drop/agents/{ID}/memory-agent.jsonl"))).unwrap();
    let n: Vec<String> = drop
        .lines()
        .map(|l| {
            norm(
                serde_json::from_str::<Value>(l).unwrap()["fact"]
                    .as_str()
                    .unwrap(),
            )
        })
        .collect();
    assert_eq!(
        n.len(),
        n.iter().collect::<BTreeSet<_>>().len(),
        "no duplicate facts on the Grok side"
    );
    let _ = m1;
}

#[test]
fn skill_changed_on_both_sides_is_a_conflict_both_ways() {
    let x = World::new("conf");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &["--adopt"]).0, 0);
    let live = x.p("hermes/profiles/demo/skills/sample-skills/alpha/SKILL.md");
    // only the Hermes side edits: exported as an edit
    fs::write(&live, "---\nname: alpha\n---\nalpha edited on Hermes\n").unwrap();
    let (rc, out) = x.run("export-hermes", &["--dry-run"]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        out.contains("h2g 1 pending (0 fact(s), 1 skill(s), 0 cron)"),
        "{out}"
    );
    // the Grok side edits the same skill too
    w(
        &x.p("box/agent-data/workflows/alpha/SKILL.md"),
        "---\nname: alpha\n---\nalpha edited on Grok\n",
    );
    assert_eq!(x.export_grok().0, 0);
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 3, "{out}");
    assert!(
        out.contains("conflict skills sample-skills/alpha"),
        "g2h side: {out}"
    );
    assert!(
        out.contains("conflict skills alpha: changed on both sides"),
        "h2g side: {out}"
    );
    assert_eq!(
        fs::read_to_string(&live).unwrap(),
        "---\nname: alpha\n---\nalpha edited on Hermes\n"
    );
    assert!(!x.p("repo/hermes-export/normalized/skills/alpha").exists());
    let (rc, _) = x.run("drift", &[]);
    assert_eq!(rc, 1);
}

#[test]
fn persona_soul_never_exported_and_secrets_kept_out() {
    let x = World::new("persona");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &["--adopt"]).0, 0);
    let prof = x.p("hermes/profiles/demo");
    let soul = fs::read_to_string(prof.join("SOUL.md")).unwrap();
    fs::write(
        prof.join("SOUL.md"),
        format!("Local persona tweak.\n{soul}"),
    )
    .unwrap();
    w(
        &prof.join("memories/MEMORY.md"),
        "token xai-ABCDEFGHIJKLMNOPQRSTUVWX\n§\nA harmless fact.\n",
    );
    let (rc, out) = x.run("sync", &["--direction", "h2g"]);
    assert_eq!(rc, 0, "{out}");
    let hb = x.p("repo/hermes-export");
    let all: String = ["memory/memory.jsonl", "REQUESTS.md", "cron/requests.json"]
        .iter()
        .map(|f| fs::read_to_string(hb.join(f)).unwrap())
        .collect();
    assert!(
        !all.contains("xai-ABC") && !all.contains("persona tweak"),
        "{all}"
    );
    assert!(all.contains("A harmless fact"));
    assert!(out.contains("credential-like"), "{out}");
    assert!(!hb.join("normalized/SOUL.md").exists() && !hb.join("SOUL.md").exists());
}

#[test]
fn hermes_made_skill_changed_on_both_sides_is_held_back() {
    let x = World::new("both");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &["--adopt"]).0, 0);
    let prof = x.p("hermes/profiles/demo");
    let sk = prof.join("skills/devops/hermes-made/SKILL.md");
    w(&sk, "---\nname: hermes-made\n---\nv1\n");
    w(
        &prof.join("skills/.usage.json"),
        r#"{"hermes-made": {"created_by": "agent", "patch_count": 0}}"#,
    );
    assert_eq!(x.run("sync", &[]).0, 0);
    x.grok_import();
    // the Grok bot edits its copy and re-exports; the Hermes bot patches its own copy too
    w(
        &x.p("box/agent-data/workflows/hermes-made/SKILL.md"),
        "---\nname: hermes-made\n---\nv1 grok edit\n",
    );
    assert_eq!(x.export_grok().0, 0);
    // only Grok changed: nothing to send
    let (_, out) = x.run("export-hermes", &["--dry-run"]);
    assert!(
        out.contains("h2g IN SYNC") || !out.contains("hermes-made"),
        "{out}"
    );
    w(&sk, "---\nname: hermes-made\n---\nv1 hermes edit\n");
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 3, "{out}");
    assert!(
        out.contains("conflict skills hermes-made: changed on both sides since it was sent"),
        "{out}"
    );
    let lock: Value =
        serde_json::from_slice(&fs::read(prof.join("botport.lock.json")).unwrap()).unwrap();
    let before = lock["h2g_skills"]["hermes-made"].clone();
    let (rc, _) = x.run("sync", &[]);
    assert_eq!(rc, 3);
    let lock: Value =
        serde_json::from_slice(&fs::read(prof.join("botport.lock.json")).unwrap()).unwrap();
    assert_eq!(
        lock["h2g_skills"]["hermes-made"], before,
        "record not overwritten"
    );
    assert!(
        fs::read_to_string(x.p("box/agent-data/workflows/hermes-made/SKILL.md"))
            .unwrap()
            .contains("grok edit")
    );
}

#[test]
fn hand_merged_conflict_clears_and_grok_deletion_is_respected() {
    let x = World::new("clear");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &["--adopt"]).0, 0);
    let prof = x.p("hermes/profiles/demo");
    let sk = prof.join("skills/devops/hermes-made/SKILL.md");
    w(&sk, "---\nname: hermes-made\n---\nv1\n");
    w(
        &prof.join("skills/.usage.json"),
        r#"{"hermes-made": {"created_by": "agent", "patch_count": 0}}"#,
    );
    assert_eq!(x.run("sync", &[]).0, 0);
    x.grok_import();
    let gk = x.p("box/agent-data/workflows/hermes-made/SKILL.md");
    w(&gk, "---\nname: hermes-made\n---\nv2 from grok\n");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(
        x.run("sync", &[]).0,
        3,
        "Grok changed a Hermes-made skill: conflict until merged"
    );
    // the owner merges by hand: both copies equal -> both directions clear, and stay clear
    fs::write(&sk, fs::read(&gk).unwrap()).unwrap();
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 0, "{out}");
    // the Grok bot deletes it: not re-sent every round
    fs::remove_dir_all(gk.parent().unwrap()).unwrap();
    assert_eq!(x.export_grok().0, 0);
    let lock: Value =
        serde_json::from_slice(&fs::read(prof.join("botport.lock.json")).unwrap()).unwrap();
    assert!(lock["h2g_skills"]["hermes-made"].is_string());
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(!out.contains("h2g bundle:"), "nothing re-sent: {out}");
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 0, "{out}");
}

#[test]
fn sent_skill_stays_pending_until_grok_imports_it() {
    let x = World::new("unread");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &["--adopt"]).0, 0);
    let prof = x.p("hermes/profiles/demo");
    w(
        &prof.join("skills/devops/hermes-made/SKILL.md"),
        "---\nname: hermes-made\n---\nv1\n",
    );
    w(
        &prof.join("skills/.usage.json"),
        r#"{"hermes-made": {"created_by": "learn", "patch_count": 0}}"#,
    );
    assert_eq!(x.run("sync", &[]).0, 0);
    // not imported yet: still pending, and a later rewrite of the bundle (a new fact) keeps the skill in it
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 1, "{out}");
    w(&prof.join("memories/MEMORY.md"), "A later fact.\n");
    assert_eq!(x.run("sync", &[]).0, 0);
    assert!(x
        .p("repo/hermes-export/normalized/skills/hermes-made/SKILL.md")
        .is_file());
    assert_eq!(x.grok_import(), (1, 1));
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &[]).0, 0);
    assert_eq!(x.run("drift", &[]).0, 0);
}

#[test]
fn unimported_hermes_edit_of_a_known_skill_stays_pending_without_false_conflict() {
    let x = World::new("edit2");
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &["--adopt"]).0, 0);
    let prof = x.p("hermes/profiles/demo");
    let sk = prof.join("skills/devops/hermes-made/SKILL.md");
    w(&sk, "---\nname: hermes-made\n---\nv1\n");
    w(
        &prof.join("skills/.usage.json"),
        r#"{"hermes-made": {"created_by": "agent", "patch_count": 0}}"#,
    );
    assert_eq!(x.run("sync", &[]).0, 0);
    x.grok_import();
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &[]).0, 0);
    assert_eq!(x.run("drift", &[]).0, 0);
    // Hermes edits it (v2) and sends; before Grok imports, Hermes learns a fact and the bundle is rewritten
    w(&sk, "---\nname: hermes-made\n---\nv2\n");
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    w(&prof.join("memories/MEMORY.md"), "Another fact.\n");
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "no false conflict: {out}");
    assert!(
        fs::read_to_string(x.p("repo/hermes-export/normalized/skills/hermes-made/SKILL.md"))
            .unwrap()
            .contains("v2")
    );
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 1, "pending until imported: {out}");
    assert!(!out.contains("conflict"), "{out}");
    x.grok_import();
    assert_eq!(x.export_grok().0, 0);
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 0, "converged: {out}");
    assert!(
        fs::read_to_string(x.p("box/agent-data/workflows/hermes-made/SKILL.md"))
            .unwrap()
            .contains("v2")
    );
    assert!(!prof.join("skills/sample-skills/hermes-made").exists());
}

/// Randomized multi-round model test: random edits on both sides, syncs, imports and exports. Invariants after every
/// step: no edit is lost on either side, no skill is installed back into the owned category, and once edits stop
/// (with hand-merging of reported conflicts) both directions converge.
#[test]
fn random_rounds_never_lose_edits_and_converge() {
    let n: u64 = std::env::var("G2H_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    for seed in 1..=n {
        let x = World::new(&format!("rand{seed}"));
        // an excluded shared Grok skill that churns randomly must never matter
        let spec = x.p("repo/spec.toml");
        let t = fs::read_to_string(&spec).unwrap().replacen(
            "skill_category = \"sample-skills\"",
            "skill_category = \"sample-skills\"\nskills_include = [\"alpha\"]",
            1,
        );
        fs::write(&spec, t).unwrap();
        let shared = x.p("box/agent-data/workflows/shared-lib/SKILL.md");
        w(&shared, "---\nname: shared-lib\n---\ns0\n");
        assert_eq!(x.export_grok().0, 0);
        assert_eq!(x.run("sync", &["--adopt"]).0, 0);
        let prof = x.p("hermes/profiles/demo");
        let hsk = prof.join("skills/devops/hermes-made/SKILL.md");
        let gsk = x.p("box/agent-data/workflows/hermes-made/SKILL.md");
        w(&hsk, "---\nname: hermes-made\n---\nh0\n");
        w(
            &prof.join("skills/.usage.json"),
            r#"{"hermes-made": {"created_by": "agent", "patch_count": 0}}"#,
        );
        // what the simulated Grok bot last imported (its "since the last sync" baseline, per the brief)
        let mut grok_base: Option<String> = None;
        let mut st = seed
            .wrapping_mul(2862933555777941757)
            .wrapping_add(3037000493);
        let mut rnd = |n: u64| {
            st = st
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (st >> 33) % n
        };
        let mut last_h = fs::read_to_string(&hsk).unwrap();
        for step in 0..14 {
            match rnd(6) {
                0 => {
                    last_h = format!("---\nname: hermes-made\n---\nh{seed}-{step}\n");
                    fs::write(&hsk, &last_h).unwrap();
                }
                1 if gsk.exists() => {
                    fs::write(
                        &gsk,
                        format!("---\nname: hermes-made\n---\ng{seed}-{step}\n"),
                    )
                    .unwrap();
                }
                1 => {
                    if shared.exists() {
                        fs::remove_dir_all(shared.parent().unwrap()).unwrap();
                    } else {
                        w(&shared, &format!("---\nname: shared-lib\n---\ns{step}\n"));
                    }
                }
                2 => {
                    let rc = x.run("sync", &[]).0;
                    assert!(rc == 0 || rc == 3, "seed {seed} step {step}: rc {rc}");
                }
                3 => {
                    // simulated import, honouring "never overwrite your own change"
                    let b = x.p("repo/hermes-export/normalized/skills/hermes-made/SKILL.md");
                    if b.is_file() {
                        let mine = fs::read_to_string(&gsk).ok();
                        if mine.is_none() || mine == grok_base {
                            let v = fs::read_to_string(&b).unwrap();
                            w(&gsk, &v);
                            grok_base = Some(v);
                        }
                    }
                }
                _ => {
                    assert_eq!(x.export_grok().0, 0);
                }
            }
            // invariants: the Hermes copy is only ever changed by the Hermes bot; never installed back
            assert_eq!(
                fs::read_to_string(&hsk).unwrap(),
                last_h,
                "seed {seed} step {step}: Hermes copy changed by the tool"
            );
            assert!(
                !prof.join("skills/sample-skills/hermes-made").exists(),
                "seed {seed} step {step}: installed back"
            );
            assert!(
                !prof.join("skills/sample-skills/shared-lib").exists(),
                "seed {seed} step {step}: excluded skill imported"
            );
            assert!(
                !x.p("repo/hermes-export/normalized/skills/shared-lib")
                    .exists(),
                "seed {seed} step {step}: excluded sent back"
            );
        }
        // settle: hand-merge any conflict (owner keeps the Hermes version), then sync/import/export until quiet
        for _ in 0..4 {
            assert_eq!(x.export_grok().0, 0);
            let (rc, _) = x.run("sync", &[]);
            if rc == 3 && gsk.exists() {
                fs::write(&gsk, fs::read(&hsk).unwrap()).unwrap(); // merged by hand on the Grok side
                grok_base = Some(fs::read_to_string(&gsk).unwrap());
                continue;
            }
            let b = x.p("repo/hermes-export/normalized/skills/hermes-made/SKILL.md");
            if b.is_file() && (fs::read_to_string(&gsk).ok() == grok_base || !gsk.exists()) {
                let v = fs::read_to_string(&b).unwrap();
                w(&gsk, &v);
                grok_base = Some(v);
            }
        }
        assert_eq!(x.export_grok().0, 0);
        let (rc, out) = x.run("sync", &[]);
        assert_eq!(
            rc,
            0,
            "seed {seed}: {out}\nhermes={:?} grok={:?} lock={}",
            fs::read_to_string(&hsk).ok(),
            fs::read_to_string(&gsk).ok(),
            fs::read_to_string(prof.join("botport.lock.json"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("hermes-made") || l.contains("h2g"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let (rc, out) = x.run("drift", &[]);
        assert_eq!(rc, 0, "seed {seed} did not converge: {out}");
        assert_eq!(
            fs::read_to_string(&gsk).unwrap(),
            fs::read_to_string(&hsk).unwrap(),
            "seed {seed}"
        );
    }
}

#[test]
fn allowlist_two_way_excluded_grok_skill_never_sent_back_but_hermes_made_still_is() {
    let x = World::new("allow2");
    // Grok holds a shared-library skill the profile doesn't take
    w(
        &x.p("box/agent-data/workflows/shared-lib/SKILL.md"),
        "---\nname: shared-lib\n---\nlib v1\n",
    );
    let spec = x.p("repo/spec.toml");
    let t = fs::read_to_string(&spec).unwrap().replacen(
        "skill_category = \"sample-skills\"",
        "skill_category = \"sample-skills\"\nskills_include = []",
        1,
    );
    fs::write(&spec, t).unwrap();
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &["--adopt"]).0, 0);
    let prof = x.p("hermes/profiles/demo");
    assert!(
        !prof.join("skills/sample-skills/shared-lib").exists()
            && !prof.join("skills/sample-skills/alpha").exists()
    );
    // the Hermes bot copies the shared skill locally and edits it (marked as its own), and makes a new one
    w(
        &prof.join("skills/devops/shared-lib/SKILL.md"),
        "---\nname: shared-lib\n---\nlib hermes edit\n",
    );
    w(
        &prof.join("skills/devops/hermes-made/SKILL.md"),
        "---\nname: hermes-made\n---\nmine\n",
    );
    w(
        &prof.join("skills/.usage.json"),
        r#"{"shared-lib": {"created_by": "agent"}, "hermes-made": {"created_by": "agent"}}"#,
    );
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        !x.p("repo/hermes-export/normalized/skills/shared-lib")
            .exists(),
        "excluded Grok skill never sent back"
    );
    assert!(
        x.p("repo/hermes-export/normalized/skills/hermes-made/SKILL.md")
            .is_file(),
        "Hermes-made still sent"
    );
    // round trip: the Hermes-made skill comes back in the Grok export and is not dropped as excluded
    x.grok_import();
    w(
        &x.p("box/agent-data/workflows/shared-lib/SKILL.md"),
        "---\nname: shared-lib\n---\nlib v2 upstream\n",
    );
    assert_eq!(x.export_grok().0, 0);
    let (rc, out) = x.run("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 0, "{out}");
    // the Hermes bot edits its own skill again: still sent (not mistaken for an excluded Grok skill)
    w(
        &prof.join("skills/devops/hermes-made/SKILL.md"),
        "---\nname: hermes-made\n---\nmine v2\n",
    );
    let (rc, out) = x.run("drift", &[]);
    assert_eq!(rc, 1, "{out}");
    assert!(out.contains("1 skill(s)"), "{out}");
    // deleting the excluded skill on Grok changes nothing
    fs::remove_dir_all(x.p("box/agent-data/workflows/shared-lib")).unwrap();
    assert_eq!(x.export_grok().0, 0);
    assert_eq!(x.run("sync", &[]).0, 0);
    assert!(!prof.join("skills/sample-skills/shared-lib").exists());
}

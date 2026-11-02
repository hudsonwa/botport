//! End-to-end tests of the binary on synthetic fixtures (no Hermes needed).
mod common;
use common::{s, w, Tmp};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_grok2hermes"));
    c.env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR")); // not ~/.local/state
    c
}

/// A legacy hand export + a port spec with templates.
fn fixture(t: &Path) -> (PathBuf, PathBuf) {
    let g = t.join("grok");
    w(
        &g.join("profile.json"),
        r#"{"name":"Demo","harness":"temporal"}"#,
    );
    w(
        &g.join("settings.json"),
        r#"{"voiceId":"voice-a","notify":true}"#,
    );
    w(&g.join("MEMORY.md"), "# Demo memory\n- port 12345\n");
    w(
        &g.join("skills/alpha/SKILL.md"),
        "---\nname: Alpha\n---\n# Alpha\\n\\nline two\n",
    );
    w(
        &g.join("skills/beta/SKILL.md"),
        "---\nname: beta\n---\nbody\n",
    );
    w(&g.join(".env"), "XAI_KEY=nothing\n");
    let tpl = t.join("tpl");
    w(&tpl.join("SOUL.md"), "You are Demo.\n");
    w(&tpl.join("config.yaml"), "# header\nmodel:\n  default: example-model\nplatform_toolsets:\n  cli: [a, b]\nweb:\n  backend: x\n");
    w(&tpl.join("USER.md"), "Springfield\n§\nplain-text notes\n");
    w(
        &tpl.join("HEADER.md"),
        "---\nname: demo-mem\n---\nrepo {{REPO}}\n",
    );
    let spec = t.join("spec.toml");
    w(
        &spec,
        &format!(
            r#"profile = "demo"
base = "{}"
skill_category = "sample-skills"
category_description = "skills: {{names}}."
disable_other_skills = true
[vars]
REPO = "/r"
[memory_skill]
name = "demo-mem"
header = "HEADER.md"
[soul]
template = "SOUL.md"
[config]
template = "config.yaml"
preserve = ["web"]
[user]
seed = "USER.md"
"#,
            tpl.display()
        ),
    );
    (g, spec)
}

fn run(args: &[&str]) -> (i32, String) {
    let o = bin().args(args).output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
    )
}

#[test]
fn export_is_deterministic_secret_free_and_verifies() {
    let t = Tmp::new("export");
    let (g, _) = fixture(&t.0);
    let (a, b) = (t.0.join("b1"), t.0.join("b2"));
    for o in [&a, &b] {
        let (rc, out) = run(&["export", "--from-dir", s(&g), "--out", s(o)]);
        assert_eq!(rc, 0, "{out}");
        assert!(out.lines().count() <= 10);
    }
    assert_eq!(
        fs::read(a.join("manifest.json")).unwrap(),
        fs::read(b.join("manifest.json")).unwrap()
    );
    assert!(!a.join("raw/.env").exists());
    assert!(fs::read_to_string(a.join("NOT_CAPTURED.md"))
        .unwrap()
        .contains("`.env`"));
    assert_eq!(
        fs::read_to_string(a.join("normalized/skills/alpha/SKILL.md")).unwrap(),
        "---\nname: Alpha\n---\n# Alpha\n\nline two\n"
    );
    // re-export over its own previous output is allowed and identical
    let (rc, _) = run(&["export", "--from-dir", s(&g), "--out", s(&a)]);
    assert_eq!(rc, 0);
    assert_eq!(
        fs::read(a.join("manifest.json")).unwrap(),
        fs::read(b.join("manifest.json")).unwrap()
    );
    assert_eq!(run(&["verify", "--bundle", s(&a)]).0, 0);
    fs::write(a.join("normalized/MEMORY.md"), "tampered").unwrap();
    let (rc, out) = run(&["verify", "--bundle", s(&a)]);
    assert_eq!(rc, 1);
    assert!(out.contains("hash mismatch: normalized/MEMORY.md"));
    // --check-routines on a failed verify: same exit code, the report kept, a skipped note added
    let (rc, out) = run(&["verify", "--bundle", s(&a), "--check-routines", REPLICA]);
    assert_eq!(rc, 1);
    assert!(out.contains("hash mismatch: normalized/MEMORY.md"), "{out}");
    assert!(
        out.contains("routine check (experimental): skipped (the command did not succeed)"),
        "{out}"
    );
    // a foreign non-empty dir is refused
    let f = t.0.join("foreign");
    w(&f.join("x.txt"), "x");
    assert_eq!(run(&["export", "--from-dir", s(&g), "--out", s(&f)]).0, 2);
}

#[test]
fn memory_drop_and_freshness_gate() {
    let t = Tmp::new("drop");
    let ad = t.0.join("agent-data");
    let id = "00000000-0000-4000-8000-0000000000a1";
    w(
        &ad.join(format!("agents/{id}/profile.json")),
        r#"{"name":"Demo"}"#,
    );
    w(&ad.join("workflows/alpha/SKILL.md"), "a\n");
    w(&ad.join("workflows/managed-skills/x/SKILL.md"), "managed\n");
    let md = t.0.join("drop");
    w(&md.join(format!("agents/{id}/memory-agent.jsonl")), "{\"tier\":\"profile\",\"fact\":\"z\",\"date\":\"2026-10-30\"}\n{\"fact\":\"a\",\"date\":\"2026-10-01\",\"tier\":\"note\"}\n");
    let out = t.0.join("b");
    let (rc, o) = run(&[
        "export",
        "--agent-data",
        s(&ad),
        "--memory-drop",
        s(&md),
        "--out",
        s(&out),
    ]);
    assert_eq!(rc, 0, "{o}");
    assert!(o.contains("memory 2 rows"), "{o}");
    let jl = fs::read_to_string(out.join("memory/memory.jsonl")).unwrap();
    assert!(
        jl.lines().next().unwrap().contains("2026-10-01"),
        "sorted by date"
    );
    assert!(!out.join("raw/skills/managed-skills/x/SKILL.md").exists());
    assert!(fs::read_to_string(out.join("normalized/MEMORY.md"))
        .unwrap()
        .contains("Persistent (profile)"));
    // stale drop: strict fails
    let (rc, o) = run(&[
        "export",
        "--agent-data",
        s(&ad),
        "--memory-drop",
        s(&md),
        "--out",
        s(&out),
        "--max-memory-age-days",
        "-1",
        "--strict",
    ]);
    assert_eq!(rc, 2, "{o}");
}

#[test]
fn apply_plan_sync_conflicts_and_restore_points() {
    let t = Tmp::new("apply");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    let prof = hh.join("profiles/demo");
    w(
        &prof.join("skills/other/thing/SKILL.md"),
        "---\nname: Other Thing\n---\n",
    );
    w(&prof.join("config.yaml"), "model:\n  default: old\nplatform_toolsets:\n  cli: [a, wiki, b]\nweb:\n  backend: mine\nlocal: keep\n");
    let base = [
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--hermes-home",
        s(&hh),
    ];
    let with = |cmd: &str, extra: &[&str]| {
        let mut v = vec![cmd];
        v.extend(base);
        v.extend(extra);
        run(&v)
    };
    let (rc, out) = with("drift", &[]);
    assert_eq!(rc, 1, "{out}");
    let (rc, out) = with("plan", &[]);
    assert_eq!(rc, 0);
    assert!(out.contains("conflict config model.default"), "{out}");
    // first apply: unrecorded config conflict is kept; --adopt takes it over
    let (rc, out) = with("apply", &[]);
    assert_eq!(rc, 3, "{out}");
    assert!(fs::read_to_string(prof.join("config.yaml"))
        .unwrap()
        .contains("default: \"old\""));
    let (rc, out) = with("apply", &["--adopt"]);
    assert_eq!(rc, 0, "{out}");
    let cfg = fs::read_to_string(prof.join("config.yaml")).unwrap();
    assert!(cfg.starts_with("# header\n"));
    assert!(
        cfg.contains("default: \"example-model\"")
            && cfg.contains("backend: \"mine\"")
            && cfg.contains("local: \"keep\""),
        "{cfg}"
    );
    assert!(
        cfg.contains("- \"wiki\""),
        "live list extras survive: {cfg}"
    );
    assert!(cfg.contains("Other Thing"), "other skills disabled: {cfg}");
    assert_eq!(
        fs::read_to_string(prof.join("skills/sample-skills/demo-mem/SKILL.md")).unwrap(),
        "---\nname: demo-mem\n---\nrepo /r\n\n# Demo memory\n- port 12345\n"
    );
    assert!(
        fs::read_to_string(prof.join("skills/sample-skills/DESCRIPTION.md"))
            .unwrap()
            .contains("alpha, beta, demo-mem")
    );
    assert!(fs::read_to_string(prof.join("memories/USER.md"))
        .unwrap()
        .contains("Springfield\n§\nplain-text notes"));
    let (rc, out) = with("drift", &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("IN SYNC"));
    // sync with nothing changed is a no-op
    let (rc, out) = with("sync", &[]);
    assert_eq!(rc, 0);
    assert!(out.contains("IN SYNC"), "{out}");
    // Hermes-side text outside the SOUL markers survives; an edited skill is a conflict, never overwritten
    let soul = fs::read_to_string(prof.join("SOUL.md")).unwrap();
    fs::write(prof.join("SOUL.md"), format!("My local rule.\n{soul}")).unwrap();
    fs::write(
        prof.join("skills/sample-skills/alpha/SKILL.md"),
        "edited by hand\n",
    )
    .unwrap();
    w(
        &g.join("skills/beta/SKILL.md"),
        "---\nname: beta\n---\nbody v2\n",
    );
    w(&t.0.join("tpl/SOUL.md"), "You are Demo v2.\n");
    let (rc, out) = with("sync", &[]);
    assert_eq!(rc, 3, "{out}");
    assert!(out.contains("conflict skills sample-skills/alpha"), "{out}");
    assert_eq!(
        fs::read_to_string(prof.join("skills/sample-skills/alpha/SKILL.md")).unwrap(),
        "edited by hand\n"
    );
    assert!(
        fs::read_to_string(prof.join("skills/sample-skills/beta/SKILL.md"))
            .unwrap()
            .contains("body v2")
    );
    let soul = fs::read_to_string(prof.join("SOUL.md")).unwrap();
    assert!(
        soul.starts_with("My local rule.\n") && soul.contains("You are Demo v2."),
        "{soul}"
    );
    // skill removed from the export -> retired to backups
    fs::remove_dir_all(g.join("skills/beta")).unwrap();
    let (_, out) = with("sync", &[]);
    assert!(out.contains("retire"), "{out}");
    assert!(!prof.join("skills/sample-skills/beta").exists());
    // restore points: at most 5 kept
    for i in 0..6 {
        w(&t.0.join("tpl/SOUL.md"), &format!("v{i}\n"));
        with("sync", &[]);
    }
    let n = fs::read_dir(prof.join("backups/botport"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".zip"))
        .count();
    assert_eq!(n, 5);
}

#[test]
fn json_output_is_parseable_and_summary_short() {
    let t = Tmp::new("json");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    let (rc, out) = run(&[
        "plan",
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--hermes-home",
        s(&hh),
        "--json",
    ]);
    assert_eq!(rc, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["in_sync"], false);
    let (_, out) = run(&[
        "plan",
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--hermes-home",
        s(&hh),
    ]);
    assert!(out.lines().count() <= 10, "{out}");
}

#[test]
fn edited_soul_section_stays_a_conflict_across_applies() {
    let t = Tmp::new("soulconf");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    let prof = hh.join("profiles/demo");
    let base = [
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--hermes-home",
        s(&hh),
    ];
    let with = |cmd: &str| {
        let mut v = vec![cmd];
        v.extend(base);
        run(&v)
    };
    assert_eq!(with("apply").0, 0);
    let soul = fs::read_to_string(prof.join("SOUL.md"))
        .unwrap()
        .replace("You are Demo.", "You are MY Demo.");
    fs::write(prof.join("SOUL.md"), &soul).unwrap();
    w(&g.join("skills/beta/SKILL.md"), "changed\n"); // another change, so apply writes something
    for _ in 0..3 {
        let (rc, out) = with("apply");
        assert_eq!(rc, 3, "{out}");
        assert!(out.contains("conflict soul"), "{out}");
    }
    assert!(fs::read_to_string(prof.join("SOUL.md"))
        .unwrap()
        .contains("You are MY Demo."));
    // unbalanced markers: conflict, untouched
    fs::write(prof.join("SOUL.md"), "<!-- botport:begin soul -->\nx\n").unwrap();
    let (rc, out) = with("apply");
    assert_eq!(rc, 3);
    assert!(out.contains("unbalanced"), "{out}");
    assert_eq!(
        fs::read_to_string(prof.join("SOUL.md")).unwrap(),
        "<!-- botport:begin soul -->\nx\n"
    );
}

#[test]
fn credential_in_history_is_kept_out() {
    let t = Tmp::new("hist");
    let (g, _) = fixture(&t.0);
    let h = t.0.join("chat.sqlite");
    w(
        &h,
        "SQLite format 3\0 ... token xai-ABCDEFGHIJKLMNOPQRSTUVWX ...",
    );
    let out = t.0.join("b");
    let (rc, o) = run(&[
        "export",
        "--from-dir",
        s(&g),
        "--with-history",
        "--history-sqlite",
        s(&h),
        "--out",
        s(&out),
    ]);
    assert_eq!(rc, 0, "{o}");
    assert!(o.contains("credential-like"), "{o}");
    assert!(!out.join("history/chat.sqlite").exists());
    // verify flags credential-like content smuggled into a bundle
    w(
        &out.join("normalized/MEMORY.md"),
        "key xai-ABCDEFGHIJKLMNOPQRSTUVWX\n",
    );
    let (rc, o) = run(&["verify", "--bundle", s(&out)]);
    assert_eq!(rc, 1);
    assert!(o.contains("credential-like content"), "{o}");
}

#[test]
fn memory_drop_credentials_redacted_and_tampered_bundle_refused() {
    let t = Tmp::new("memsec");
    let ad = t.0.join("agent-data");
    let id = "00000000-0000-4000-8000-0000000000a1";
    w(
        &ad.join(format!("agents/{id}/profile.json")),
        r#"{"name":"Demo"}"#,
    );
    let md = t.0.join("drop");
    w(&md.join(format!("agents/{id}/memory-agent.jsonl")), "{\"tier\":\"note\",\"fact\":\"key xai-ABCDEFGHIJKLMNOPQRSTUVWX\"}\n{\"tier\":\"note\",\"fact\":\"fine\"}\n");
    let out = t.0.join("b");
    let (rc, o) = run(&[
        "export",
        "--agent-data",
        s(&ad),
        "--memory-drop",
        s(&md),
        "--out",
        s(&out),
    ]);
    assert_eq!(rc, 0, "{o}");
    assert!(o.contains("1 row(s) redacted"), "{o}");
    for f in ["memory/memory.jsonl", "normalized/MEMORY.md"] {
        assert!(
            !fs::read_to_string(out.join(f)).unwrap().contains("xai-ABC"),
            "{f}"
        );
    }
    assert_eq!(run(&["verify", "--bundle", s(&out)]).0, 0);
    w(&out.join("normalized/MEMORY.md"), "tampered\n");
    let hh = t.0.join("hermes");
    let (rc, o) = run(&[
        "apply",
        "--bundle",
        s(&out),
        "--profile",
        "x",
        "--hermes-home",
        s(&hh),
    ]);
    assert_eq!(rc, 2, "{o}");
    assert!(o.contains("fails verify"), "{o}");
    assert!(!hh.exists());
}

#[test]
fn config_written_for_pyyaml() {
    let t = Tmp::new("yaml11");
    let (g, spec) = fixture(&t.0);
    w(
        &t.0.join("tpl/config.yaml"),
        "mode: \"off\"\nflag: off\ntime: \"8:00\"\n",
    );
    let hh = t.0.join("hermes");
    assert_eq!(
        run(&[
            "apply",
            "--bundle",
            s(&g),
            "--spec",
            s(&spec),
            "--hermes-home",
            s(&hh)
        ])
        .0,
        0
    );
    let cfg = fs::read_to_string(hh.join("profiles/demo/config.yaml")).unwrap();
    assert!(
        cfg.contains("mode: \"off\"")
            && cfg.contains("flag: false")
            && cfg.contains("time: \"8:00\""),
        "{cfg}"
    );
    assert_eq!(
        run(&[
            "drift",
            "--bundle",
            s(&g),
            "--spec",
            s(&spec),
            "--hermes-home",
            s(&hh)
        ])
        .0,
        0
    );
}

#[test]
fn json_status_words_match_summary_words() {
    let t = Tmp::new("words");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    assert_eq!(
        run(&[
            "apply",
            "--bundle",
            s(&g),
            "--spec",
            s(&spec),
            "--hermes-home",
            s(&hh)
        ])
        .0,
        0
    );
    let (_, out) = run(&[
        "plan",
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--hermes-home",
        s(&hh),
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["status"] == "in-sync"),
        "{out}"
    );
}

#[test]
fn allowlist_imports_only_listed_skills_without_churn_and_retires_excluded() {
    let t = Tmp::new("allow");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    let base = [
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--hermes-home",
        s(&hh),
    ];
    let with = |cmd: &str, extra: &[&str]| {
        let mut v = vec![cmd];
        v.extend(base);
        v.extend(extra);
        run(&v)
    };
    // first without an allowlist: alpha and beta installed
    assert_eq!(with("apply", &[]).0, 0);
    let prof = hh.join("profiles/demo");
    assert!(prof.join("skills/sample-skills/beta/SKILL.md").exists());
    // allowlist alpha only: beta is retired to backups (never deleted), alpha stays
    let text = fs::read_to_string(&spec).unwrap().replacen(
        "skill_category = \"sample-skills\"",
        "skill_category = \"sample-skills\"\nskills_include = [\"alpha\"]",
        1,
    );
    fs::write(&spec, text).unwrap();
    let (rc, out) = with("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("retire beta"), "{out}");
    assert!(!prof.join("skills/sample-skills/beta").exists());
    assert!(fs::read_dir(prof.join("backups/botport"))
        .unwrap()
        .flatten()
        .any(|e| e.path().join("sample-skills/beta/SKILL.md").exists()));
    assert!(prof.join("skills/sample-skills/alpha/SKILL.md").exists());
    let desc = fs::read_to_string(prof.join("skills/sample-skills/DESCRIPTION.md")).unwrap();
    assert!(!desc.contains("beta"), "{desc}");
    assert_eq!(with("drift", &[]).0, 0);
    // the excluded skill changing or disappearing in later exports is invisible: no churn, no pending, no conflict
    let lock1 = fs::read(prof.join("botport.lock.json")).unwrap();
    w(
        &g.join("skills/beta/SKILL.md"),
        "---\nname: beta\n---\nchanged upstream\n",
    );
    let (rc, out) = with("drift", &[]);
    assert_eq!(rc, 0, "{out}");
    let (rc, out) = with("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    assert_eq!(
        fs::read(prof.join("botport.lock.json")).unwrap(),
        lock1,
        "no re-sync for an excluded skill"
    );
    fs::remove_dir_all(g.join("skills/beta")).unwrap();
    assert_eq!(with("drift", &[]).0, 0);
    // an empty list imports no Grok skills at all
    let text = fs::read_to_string(&spec)
        .unwrap()
        .replace("skills_include = [\"alpha\"]", "skills_include = []");
    fs::write(&spec, text).unwrap();
    let (rc, out) = with("sync", &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        !prof.join("skills/sample-skills/alpha").exists()
            && prof.join("skills/sample-skills/demo-mem/SKILL.md").exists(),
        "{out}"
    );
}

#[test]
fn battery_init_is_stable_and_covers_every_category() {
    let t = Tmp::new("binit");
    let acme = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/acme");
    let (a, b) = (t.0.join("a.json"), t.0.join("b.json"));
    for out in [&a, &b] {
        let (rc, o) = run(&[
            "battery",
            "init",
            "--bundle",
            s(&acme.join("grok")),
            "--spec",
            s(&acme.join("botport.toml")),
            "--out",
            s(out),
        ]);
        assert_eq!(rc, 0, "{o}");
        assert!(o.lines().count() <= 10, "{o}");
    }
    let (ta, tb) = (fs::read(&a).unwrap(), fs::read(&b).unwrap());
    assert_eq!(ta, tb);
    let v: serde_json::Value = serde_json::from_slice(&ta).unwrap();
    let cases = v["cases"].as_array().unwrap();
    let fams: std::collections::BTreeSet<&str> = cases
        .iter()
        .map(|c| c["family"].as_str().unwrap())
        .collect();
    for f in [
        "persona",
        "skill",
        "schedule",
        "connector",
        "memory",
        "live",
    ] {
        assert!(fams.contains(f), "missing {f}: {fams:?}");
    }
    assert!(cases
        .iter()
        .all(|c| c["generated"] == true && !c["why"].as_str().unwrap().is_empty()));
    let text = String::from_utf8(ta).unwrap();
    assert!(
        !text.contains(s(&t.0))
            && !text.contains(env!("CARGO_MANIFEST_DIR"))
            && !text.contains("examples"),
        "{text}"
    );
    assert!(text.contains("Acme"));
    // names come from the structured description, not the whole bullet
    let ids: Vec<&str> = cases.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"schedule-weekly-summary") && ids.contains(&"connector-visitor-log"),
        "{ids:?}"
    );
}

#[test]
fn battery_init_respects_skills_include() {
    let t = Tmp::new("binc");
    let (g, spec) = fixture(&t.0);
    let mut sp = fs::read_to_string(&spec).unwrap();
    sp = sp.replacen(
        "skill_category",
        "skills_include = [\"beta\"]\nskill_category",
        1,
    );
    w(&spec, &sp);
    let out = t.0.join("b.json");
    let (rc, o) = run(&[
        "battery",
        "init",
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--out",
        s(&out),
    ]);
    assert_eq!(rc, 0, "{o}");
    let text = fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("skill-beta") && !text.contains("skill-alpha"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn test_command_reports_pass_fail_and_turn_limit_with_fake_hermes() {
    use std::os::unix::fs::PermissionsExt;
    let t = Tmp::new("bfake");
    let fake = t.0.join("hermes");
    w(
        &fake,
        r#"#!/bin/sh
case "$*" in
  *Who*) echo '{"type":"text","text":"I am Acme Helper."}' ;;
  *time*) echo '{"type":"tool_use","name":"terminal"}'; echo '{"type":"result","exit_code":1,"text":"ran out"}'; exit 1 ;;
  *) echo '{"type":"text","text":"no idea"}' ;;
esac
"#,
    );
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let bat = t.0.join("battery.json");
    w(
        &bat,
        r#"{"gap_s":0,"cases":[
      {"id":"persona","family":"persona","prompt":"Who are you?","patterns":["Acme"]},
      {"id":"live-time","family":"live","prompt":"What time is it?","want":["*"],"patterns":["\\d"]},
      {"id":"memory-1","family":"memory","prompt":"When open?","patterns":["09"]}]}"#,
    );
    let (rc, o) = run(&[
        "test",
        "--profile",
        "anybot",
        "--battery",
        s(&bat),
        "--hermes-bin",
        s(&fake),
        "--out",
        s(&t.0.join("out")),
    ]);
    assert_eq!(rc, 1, "{o}");
    let stdout = String::from_utf8(
        bin()
            .args([
                "test",
                "--profile",
                "anybot",
                "--battery",
                s(&bat),
                "--hermes-bin",
                s(&fake),
                "--out",
                s(&t.0.join("out")),
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(stdout.lines().count() <= 10, "{stdout}");
    assert!(stdout.contains("1/3 pass"), "{stdout}");
    assert!(stdout.contains("PASS persona"), "{stdout}");
    assert!(
        stdout.contains("FAIL live-time") && stdout.contains("tool-turn limit"),
        "{stdout}"
    );
    assert!(stdout.contains("FAIL memory-1"), "{stdout}");
}

#[test]
fn describe_writes_json_and_export_bundles_it() {
    let t = Tmp::new("describe");
    let (g, _) = fixture(&t.0);
    let desc = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/describe/structured.md");
    let dj = t.0.join("describe.json");
    let (rc, out) = run(&["describe", "--describe", s(&desc), "--out", s(&dj)]);
    assert_eq!(rc, 0, "{out}");
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&dj).unwrap()).unwrap();
    assert_eq!(v["schema"], 1);
    let b = t.0.join("bundle");
    let (rc, out) = run(&[
        "export",
        "--from-dir",
        s(&g),
        "--describe",
        s(&desc),
        "--out",
        s(&b),
    ]);
    assert_eq!(rc, 0, "{out}");
    assert!(b.join("normalized/describe.json").is_file());
    let m = fs::read_to_string(b.join("manifest.json")).unwrap();
    assert!(m.contains("normalized/describe.json"), "{m}");
    assert_eq!(run(&["verify", "--bundle", s(&b)]).0, 0);
}

#[test]
fn prose_mentions_of_secrets_survive_export_and_verify() {
    let t = Tmp::new("prose");
    let g = t.0.join("grok");
    w(&g.join("profile.json"), r#"{"name":"Demo"}"#);
    w(
        &g.join("skills/notes/SKILL.md"),
        "---\nname: notes\n---\nLog in to the portal (the password is in the vault app) and keep the api secret out of chat.\n",
    );
    // as in a real raw/ export: the memory drop sits inside the from-dir
    let drop = g.join("memory-drop");
    w(
        &drop.join("memory-agent.jsonl"),
        "{\"date\":\"2026-11-01\",\"tier\":\"core\",\"scope\":\"user\",\"source\":\"chat\",\"fact\":\"the user keeps the router password and the api secret in the vault app\"}\n",
    );
    let o = t.0.join("b");
    let (rc, out) = run(&[
        "export",
        "--from-dir",
        s(&g),
        "--memory-drop",
        s(&drop),
        "--out",
        s(&o),
    ]);
    assert_eq!(rc, 0, "{out}");
    assert!(o.join("normalized/skills/notes/SKILL.md").is_file());
    assert!(o.join("raw/memory-drop/memory-agent.jsonl").is_file());
    let (rc, out) = run(&["verify", "--bundle", s(&o)]);
    assert_eq!(rc, 0, "{out}");
}

#[test]
fn battery_init_accepts_a_grok_bot_export_file() {
    let t = Tmp::new("bexp");
    let exp = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grok-export/grok-bot-export.json");
    let acme = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/acme/grok");
    let (a, b) = (t.0.join("exp.json"), t.0.join("bun.json"));
    // positional path and --bundle both take the export file
    let (rc, o) = run(&["battery", "init", s(&exp), "--out", s(&a)]);
    assert_eq!(rc, 0, "{o}");
    let (rc, o) = run(&[
        "battery",
        "init",
        "--bundle",
        s(&exp),
        "--out",
        s(&t.0.join("exp2.json")),
    ]);
    assert_eq!(rc, 0, "{o}");
    assert_eq!(
        fs::read(&a).unwrap(),
        fs::read(t.0.join("exp2.json")).unwrap()
    );
    let (rc, o) = run(&["battery", "init", s(&acme), "--out", s(&b)]);
    assert_eq!(rc, 0, "{o}");
    let ve: serde_json::Value = serde_json::from_slice(&fs::read(&a).unwrap()).unwrap();
    let vb: serde_json::Value = serde_json::from_slice(&fs::read(&b).unwrap()).unwrap();
    let keys = |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&ve), keys(&vb));
    let cases = ve["cases"].as_array().unwrap();
    let fams: std::collections::BTreeSet<&str> = cases
        .iter()
        .map(|c| c["family"].as_str().unwrap())
        .collect();
    for f in ["persona", "skill", "memory", "live"] {
        assert!(fams.contains(f), "missing {f}: {fams:?}");
    }
    let ck = |c: &serde_json::Value| c.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(ck(&cases[0]), ck(&vb["cases"][0]));
    let text = fs::read_to_string(&a).unwrap();
    assert!(
        text.contains("Nova") && !text.contains(env!("CARGO_MANIFEST_DIR")),
        "{text}"
    );
}

#[test]
fn battery_init_rejects_a_file_that_is_not_an_export() {
    let t = Tmp::new("bbad");
    let bad = t.0.join("notes.json");
    w(&bad, "{\"hello\": 1}");
    let (rc, o) = run(&["battery", "init", s(&bad), "--out", s(&t.0.join("o.json"))]);
    assert_ne!(rc, 0, "{o}");
    assert!(
        o.contains("grok-bot-export") && o.contains("bundle directory"),
        "{o}"
    );
    assert!(!t.0.join("o.json").exists());
}

#[test]
fn botport_and_grok2hermes_binaries_behave_the_same() {
    let t = Tmp::new("alias");
    let (g, spec) = fixture(&t.0);
    let b = t.0.join("b");
    let (rc, out) = run(&["export", "--from-dir", s(&g), "--out", s(&b)]);
    assert_eq!(rc, 0, "{out}");
    let prof = t.0.join("prof");
    fs::create_dir_all(&prof).unwrap();
    // the old spec filename (grok2hermes.toml) keeps working under both names
    let spec = {
        let old = t.0.join("grok2hermes.toml");
        fs::copy(&spec, &old).unwrap();
        old
    };
    let go = |exe: &str, args: &[&str]| {
        let o = Command::new(exe)
            .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR"))
            .args(args)
            .output()
            .unwrap();
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        )
    };
    let plan = [
        "plan",
        "--bundle",
        s(&b),
        "--spec",
        s(&spec),
        "--profile-dir",
        s(&prof),
        "--json",
    ];
    for args in [&["--help"][..], &plan[..]] {
        let new = go(env!("CARGO_BIN_EXE_botport"), args);
        let old = go(env!("CARGO_BIN_EXE_grok2hermes"), args);
        assert_eq!(new, old, "{args:?}");
    }
    assert!(go(env!("CARGO_BIN_EXE_botport"), &["--help"])
        .1
        .starts_with("botport "));
}

/// A stand-in `hermes` that prints the given version line.
fn fake_hermes(dir: &Path, version: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("hermes");
    w(&p, &format!("#!/bin/sh\necho '{version}'\n"));
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn doctor_passes_on_a_ready_machine() {
    let t = Tmp::new("doctor-ok");
    let hh = t.0.join("hermes-home");
    fs::create_dir_all(hh.join("profiles/bot")).unwrap();
    let hb = fake_hermes(&t.0, "Hermes Agent v0.22.5 (2026.9.24) · stable");
    let (_, spec) = fixture(&t.0);
    let o = bin()
        .args(["doctor", "--profile", "bot", "--json", "--details"])
        .arg(t.0.join("d.json"))
        .arg("--hermes-home")
        .arg(&hh)
        .arg("--hermes-bin")
        .arg(&hb)
        .arg("--spec")
        .arg(&spec)
        .env_remove("HERMES_HOME")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{s}");
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["checks"].as_array().unwrap().len(), 4);
}

#[test]
fn doctor_fails_with_fix_hints() {
    let t = Tmp::new("doctor-bad");
    let hb = fake_hermes(&t.0, "Hermes Agent v0.10.0");
    let o = bin()
        .args(["doctor", "--profile", "nope", "--details"])
        .arg(t.0.join("d.json"))
        .env("HERMES_HOME", t.0.join("absent"))
        .arg("--hermes-bin")
        .arg(&hb)
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(1), "{s}");
    assert!(
        s.contains("[FAIL] hermes") && s.contains("hermes update"),
        "{s}"
    );
    assert!(
        s.contains("[FAIL] profile dir") && s.contains("fix:"),
        "{s}"
    );
    assert!(
        s.contains("absent/profiles/nope"),
        "HERMES_HOME honoured: {s}"
    );

    let o = bin()
        .args(["doctor", "--hermes-bin", "/nonexistent/hermes", "--details"])
        .arg(t.0.join("d.json"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stdout).contains("put `hermes` on PATH"));
}

#[test]
fn details_default_under_user_state_dir_not_tmp() {
    let t = Tmp::new("details-default");
    let hb = fake_hermes(&t.0, "Hermes Agent v0.22.5");
    let o = bin()
        .args(["doctor", "--profile", "nope", "--hermes-bin"])
        .arg(&hb)
        .env("HOME", &t.0)
        .env_remove("XDG_STATE_HOME")
        .env("HERMES_HOME", t.0.join("absent"))
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let want = t.0.join(".local/state/botport/doctor-nope.json");
    assert!(s.contains(&format!("details: {}", want.display())), "{s}");
    assert!(want.exists());
}

#[test]
fn doctor_with_spec_says_apply_creates_the_profile() {
    let t = Tmp::new("doctor-spec");
    let hb = fake_hermes(&t.0, "Hermes Agent v0.22.5");
    let (_, spec) = fixture(&t.0);
    let o = bin()
        .args(["doctor", "--profile", "fresh", "--spec"])
        .arg(&spec)
        .arg("--hermes-bin")
        .arg(&hb)
        .arg("--details")
        .arg(t.0.join("d.json"))
        .env("HERMES_HOME", t.0.join("hh"))
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("apply will create it"), "{s}");
    assert!(!s.contains("hermes profile create"), "{s}");
}

#[test]
fn version_flag_prints_name_and_version() {
    for f in ["--version", "-V"] {
        let o = bin().arg(f).output().unwrap();
        assert!(o.status.success(), "{f}");
        let s = String::from_utf8(o.stdout).unwrap();
        assert_eq!(s, format!("botport {}\n", env!("CARGO_PKG_VERSION")), "{f}");
    }
}

#[test]
fn verify_explains_a_legacy_hand_export() {
    let t = Tmp::new("verify-legacy");
    w(&t.0.join("profile.json"), r#"{"name":"Demo"}"#);
    let o = bin()
        .args(["verify", "--bundle", t.0.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    let e = String::from_utf8(o.stderr).unwrap();
    assert!(
        e.contains(
            "no manifest.json: this is a legacy hand export; verify needs an `export` bundle"
        ),
        "{e}"
    );
}

#[test]
fn pre_rename_profile_reads_in_sync_and_next_sync_migrates_to_botport_names_once() {
    let t = Tmp::new("legacy");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    let args = |cmd: &'static str| {
        vec![
            cmd,
            "--bundle",
            s(&g),
            "--spec",
            s(&spec),
            "--hermes-home",
            s(&hh),
        ]
    };
    assert_eq!(run(&args("apply")).0, 0);
    let prof = hh.join("profiles/demo");
    // turn it into a profile written before the rename
    fs::rename(
        prof.join("botport.lock.json"),
        prof.join("grok2hermes.lock.json"),
    )
    .unwrap();
    w(
        &prof.join("grok2hermes-manual.md"),
        "# Set up by hand\n\n- [x] ticked by the user\n",
    );
    let soul = fs::read_to_string(prof.join("SOUL.md")).unwrap();
    assert!(soul.contains("<!-- botport:begin "), "{soul}");
    let old_soul = soul.replace("<!-- botport:", "<!-- grok2hermes:");
    fs::write(prof.join("SOUL.md"), &old_soul).unwrap();
    // read-only commands: in sync, no spurious diff, nothing touched
    let (rc, out) = run(&args("drift"));
    assert_eq!(rc, 0, "{out}");
    assert_eq!(fs::read_to_string(prof.join("SOUL.md")).unwrap(), old_soul);
    assert!(
        prof.join("grok2hermes.lock.json").is_file() && !prof.join("botport.lock.json").exists()
    );
    // the next sync migrates: new names written, old ones gone, user ticks kept
    let (rc, out) = run(&args("sync"));
    assert_eq!(rc, 0, "{out}");
    assert_eq!(fs::read_to_string(prof.join("SOUL.md")).unwrap(), soul);
    assert!(
        prof.join("botport.lock.json").is_file() && !prof.join("grok2hermes.lock.json").exists()
    );
    assert!(fs::read_to_string(prof.join("botport-manual.md"))
        .unwrap()
        .contains("- [x] ticked"));
    assert!(!prof.join("grok2hermes-manual.md").exists());
    assert!(prof.join("backups/botport").is_dir());
    // idempotent: a second sync changes nothing
    let snap = |f: &str| fs::read(prof.join(f)).unwrap();
    let before = (
        snap("SOUL.md"),
        snap("botport.lock.json"),
        fs::read_dir(prof.join("backups/botport")).unwrap().count(),
    );
    let (rc, out) = run(&args("sync"));
    assert_eq!(rc, 0, "{out}");
    assert_eq!(
        before,
        (
            snap("SOUL.md"),
            snap("botport.lock.json"),
            fs::read_dir(prof.join("backups/botport")).unwrap().count()
        )
    );
    assert_eq!(run(&args("drift")).0, 0);
}

/// A stand-in `hermes` for `cron create`: logs each call to `$HERMES_HOME/profiles/P/cron/calls.log`.
fn cron_hermes(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("cron-hermes");
    w(
        &p,
        r#"#!/bin/sh
[ "$1" = --version ] && { echo 'Hermes Agent v0.22.5'; exit 0; }
prof=$2; shift 3; sub=$1; shift
d="$HERMES_HOME/profiles/$prof/cron"; mkdir -p "$d"
echo "$sub $*" >> "$d/calls.log"
"#,
    );
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn acme_apply(t: &Path, hermes_bin: &Path) -> (i32, String, PathBuf) {
    let acme = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/acme");
    let hh = t.join("hermes-home");
    fs::create_dir_all(&hh).unwrap();
    let o = bin()
        .args(["apply", "--bundle", s(&acme.join("grok")), "--spec"])
        .arg(acme.join("botport.toml"))
        .arg("--hermes-home")
        .arg(&hh)
        .arg("--hermes-bin")
        .arg(hermes_bin)
        .arg("--details")
        .arg(t.join("d.json"))
        .env_remove("HERMES_HOME")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    (
        o.status.code().unwrap_or(-1),
        out,
        hh.join("profiles/acme-helper"),
    )
}

#[test]
fn acme_disabled_cron_job_is_created_paused() {
    let t = Tmp::new("acme-paused");
    let hb = cron_hermes(&t.0);
    let (rc, out, dest) = acme_apply(&t.0, &hb);
    assert_eq!(rc, 0, "{out}");
    let log = fs::read_to_string(dest.join("cron/calls.log")).unwrap();
    let weekly = log
        .lines()
        .find(|l| l.contains("acme-weekly-report"))
        .unwrap();
    assert!(weekly.contains("--paused"), "{log}");
    let morning = log
        .lines()
        .find(|l| l.contains("acme-morning-note"))
        .unwrap();
    assert!(!morning.contains("--paused"), "{log}");
}

#[test]
fn every_created_cron_job_is_paused_unless_explicitly_enabled() {
    let t = Tmp::new("acme-all-paused");
    let hb = cron_hermes(&t.0);
    let (rc, out, dest) = acme_apply(&t.0, &hb);
    assert_eq!(rc, 0, "{out}");
    let spec: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string("examples/acme/template/cron.yaml").unwrap())
            .unwrap();
    let enabled: Vec<&str> = spec["jobs"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter(|j| j["enabled"].as_bool() == Some(true))
        .map(|j| j["name"].as_str().unwrap())
        .collect();
    let log = fs::read_to_string(dest.join("cron/calls.log")).unwrap();
    let creates: Vec<&str> = log.lines().filter(|l| l.contains("create")).collect();
    assert!(!creates.is_empty(), "{log}");
    for l in creates {
        let live = enabled.iter().any(|n| l.contains(n));
        assert_eq!(
            !l.contains("--paused"),
            live,
            "paused only unless enabled: true: {l}"
        );
    }
}

#[test]
fn plan_and_apply_name_the_destination_on_the_first_line() {
    let t = Tmp::new("acme-dest");
    let hb = cron_hermes(&t.0);
    let (rc, out, dest) = acme_apply(&t.0, &hb);
    assert_eq!(rc, 0, "{out}");
    let first = out.lines().next().unwrap();
    assert!(
        first.starts_with("apply acme-helper") && first.contains(s(&dest)),
        "{out}"
    );
}

#[test]
fn apply_fails_before_any_write_when_cron_needs_a_missing_hermes() {
    let t = Tmp::new("acme-nohermes");
    let (rc, out, dest) = acme_apply(&t.0, &t.0.join("no-such-hermes"));
    assert_eq!(rc, 2, "{out}");
    assert!(out.contains("fix:"), "{out}");
    assert!(
        !dest.exists(),
        "wrote {} before failing: {out}",
        dest.display()
    );
}

#[test]
fn bare_botport_prints_usage_to_stderr_and_exits_2() {
    let o = bin().output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    assert!(String::from_utf8_lossy(&o.stderr).contains("botport"));
}

#[test]
fn dash_h_prints_help() {
    let o = bin().arg("-h").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).contains("botport <command>"));
}

#[test]
fn doctor_without_a_profile_writes_doctor_json() {
    let t = Tmp::new("doctor-noprofile");
    let hb = fake_hermes(&t.0, "Hermes Agent v0.22.5");
    let o = bin()
        .args(["doctor", "--hermes-bin"])
        .arg(&hb)
        .env("HOME", &t.0)
        .env_remove("XDG_STATE_HOME")
        .env("HERMES_HOME", t.0.join("absent"))
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let want = t.0.join(".local/state/botport/doctor.json");
    assert!(s.contains(&format!("details: {}", want.display())), "{s}");
}

#[test]
fn describe_out_dash_writes_json_to_stdout() {
    let desc = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/describe/structured.md");
    let o = bin()
        .args(["describe", "--describe", s(&desc), "--out", "-"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&o.stdout)));
    assert_eq!(v["schema"], 1);
    assert!(String::from_utf8_lossy(&o.stderr).contains("describe:"));
    assert!(!Path::new("-").exists());
}

/// `apply --no-cron` then `drift --no-cron` is in sync, as `sync --no-cron` is (cron is left out of both).
#[test]
fn drift_no_cron_matches_apply_no_cron() {
    let t = Tmp::new("acme-nocron-drift");
    let hb = fake_hermes(&t.0, "Hermes Agent v0.22.5");
    let acme = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/acme");
    let hh = t.0.join("hermes-home");
    fs::create_dir_all(&hh).unwrap();
    let go = |cmd: &str| {
        bin()
            .args([cmd, "--bundle", s(&acme.join("grok")), "--spec"])
            .arg(acme.join("botport.toml"))
            .arg("--hermes-home")
            .arg(&hh)
            .arg("--hermes-bin")
            .arg(&hb)
            .arg("--no-cron")
            .arg("--details")
            .arg(t.0.join("d.json"))
            .env_remove("HERMES_HOME")
            .output()
            .unwrap()
    };
    assert_eq!(go("apply").status.code(), Some(0));
    let o = go("drift");
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
}

#[test]
fn usage_lists_no_cron_for_drift() {
    let o = bin().arg("--help").output().unwrap();
    let u = String::from_utf8_lossy(&o.stdout);
    let drift = u
        .lines()
        .find(|l| l.trim_start().starts_with("drift"))
        .unwrap();
    assert!(drift.contains("--no-cron"), "{drift}");
}

#[test]
fn drift_does_not_flag_spec_jobs_created_paused() {
    let t = Tmp::new("acme-paused-drift");
    let hb = cron_hermes(&t.0);
    let (rc, out, dest) = acme_apply(&t.0, &hb);
    assert_eq!(rc, 0, "{out}");
    // what Hermes holds for the spec's jobs after that apply: one live, one created paused
    let job = |name: &str, expr: &str, prompt: &str, on: bool| {
        serde_json::json!({"name": name, "prompt": prompt, "deliver": "local",
            "schedule": {"kind": "cron", "expr": expr}, "enabled": on})
    };
    let jobs = serde_json::json!({"jobs": [
        job("acme-morning-note", "0 9 * * 1-5", "Say good morning in one line.", true),
        job("acme-weekly-report", "0 16 * * 5",
            "Summarise this week's helpdesk questions in three bullets (not enabled, so it is created paused).", false),
    ]});
    w(&dest.join("cron/jobs.json"), &jobs.to_string());
    let acme = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/acme");
    let o = bin()
        .args(["drift", "--bundle", s(&acme.join("grok")), "--spec"])
        .arg(acme.join("botport.toml"))
        .arg("--hermes-home")
        .arg(t.0.join("hermes-home"))
        .arg("--hermes-bin")
        .arg(&hb)
        .arg("--details")
        .arg(t.0.join("d2.json"))
        .env_remove("HERMES_HOME")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    // the bot's own schedule job is absent from this hand-written jobs.json, so it is the one "create"
    assert!(!out.contains("paused/disabled"), "{out}");
    assert!(!out.contains("conflict"), "{out}");
    assert!(out.contains(" 1 create "), "{out}");
}

const REPLICA: &str = "tests/fixtures/routines-cache/onqw4zbomnwgszlooqxhg3djmnss4yldmnxxk3tufztgs6duovzgkltuojqw443dojuxa5boojsxa3djmnqxgltsge.blob";

#[test]
fn verify_check_routines_warns_on_missing_and_never_fails() {
    let exp = "tests/fixtures/grok-export/grok-bot-export.json";
    let out = bin()
        .args(["verify", "--bundle", exp, "--check-routines", REPLICA])
        .output()
        .unwrap();
    assert!(out.status.success());
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(so.contains("WARN routine \"Weekly Review\""), "{so}");
    assert!(!so.contains("Sample Report\" is in"), "{so}");
    // absent cache: a note, exit 0
    let out = bin()
        .args([
            "verify",
            "--bundle",
            exp,
            "--check-routines",
            "tests/fixtures/routines-cache/absent",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("skipped (no Grok desktop cache found)"));
    // a non-replica file is refused unopened, exit 0
    let out = bin()
        .args(["verify", "--bundle", exp, "--check-routines", "Cargo.toml"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout)
        .contains("skipped (not a Grok transcript-replica file"));
    // commands that ignore the flag refuse it
    let out = bin()
        .args(["describe", "--bundle", exp, "--check-routines", "auto"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--check-routines works only with"));
}

#[test]
fn profile_names_must_be_plain() {
    let t = Tmp::new("profname");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    for bad in ["../escape", "a/b", ".hidden", ".."] {
        for cmd in ["plan", "apply"] {
            let (rc, out) = run(&[
                cmd,
                "--bundle",
                s(&g),
                "--spec",
                s(&spec),
                "--hermes-home",
                s(&hh),
                "--profile",
                bad,
            ]);
            assert_ne!(rc, 0, "{cmd} {bad}: {out}");
            assert!(out.contains("plain name"), "{cmd} {bad}: {out}");
        }
    }
    assert!(!t.0.join("escape").exists());
    assert!(!hh.join("profiles").exists());
}

#[cfg(unix)]
#[test]
fn unlisted_symlinked_skill_dir_is_refused_by_verify_plan_and_apply() {
    let t = Tmp::new("symskill");
    let (g, spec) = fixture(&t.0);
    let hh = t.0.join("hermes");
    w(
        &t.0.join("outside/evil/SKILL.md"),
        "---\nname: evil\n---\nx\n",
    );
    w(&t.0.join("outside/evil/diary.txt"), "private\n");
    let b = t.0.join("B");
    let (rc, out) = run(&["export", "--from-dir", s(&g), "--out", s(&b)]);
    assert_eq!(rc, 0, "{out}");
    let (rc, out) = run(&["verify", "--bundle", s(&b)]);
    assert_eq!(rc, 0, "{out}");
    std::os::unix::fs::symlink(t.0.join("outside/evil"), b.join("normalized/skills/evil")).unwrap();
    // the legacy layout gets the same no-follow check
    std::os::unix::fs::symlink(t.0.join("outside/evil"), g.join("skills/evil")).unwrap();
    let (rc, out) = run(&["verify", "--bundle", s(&b)]);
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("normalized/skills/evil"), "{out}");
    for bundle in [&b, &g] {
        for cmd in ["plan", "apply"] {
            let (rc, out) = run(&[
                cmd,
                "--bundle",
                s(bundle),
                "--spec",
                s(&spec),
                "--hermes-home",
                s(&hh),
            ]);
            assert_ne!(rc, 0, "{cmd} {}: {out}", bundle.display());
            assert!(out.contains("skills/evil"), "{cmd}: {out}");
        }
    }
    assert!(!hh.join("profiles").exists());
}

#[test]
fn test_profile_name_must_be_plain() {
    let t = Tmp::new("testprof");
    let (rc, out) = run(&[
        "test",
        "--profile",
        "../escape",
        "--battery",
        s(&t.0.join("none.json")),
        "--hermes-bin",
        "false",
    ]);
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("plain name"), "{out}");
}

/// A stand-in `hermes` that stores jobs the way `create_job` does: an interval keeps `every <minutes>m` as its
/// display, a day/time phrase is kept as its cron expression. `edit` is only logged.
fn storing_hermes(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("storing-hermes");
    w(
        &p,
        r#"#!/bin/sh
[ "$1" = --version ] && { echo 'Hermes Agent v0.22.5'; exit 0; }
prof=$2; shift 3; sub=$1; shift
d="$HERMES_HOME/profiles/$prof/cron"; mkdir -p "$d/j"
echo "$sub $*" >> "$d/calls.log"
[ "$sub" = create ] || exit 0
en=true
while :; do case $1 in --name) name=$2; shift 2;; --deliver) shift 2;; --paused) en=false; shift;;
  --paused-reason) shift 2;; *) break;; esac; done
case $1 in
  'every 2h'|'every 120m') sch='{"kind":"interval","minutes":120,"display":"every 120m"}'; disp='every 120m';;
  'every friday 16:00'|'0 16 * * 5') sch='{"kind":"cron","expr":"0 16 * * 5"}'; disp='0 16 * * 5';;
  *) echo "unexpected schedule $1" >&2; exit 1;;
esac
n=$(ls "$d/j" | wc -l | tr -d ' ')
printf '{"id":"j%s","name":"%s","prompt":"%s","deliver":"local","enabled":%s,"schedule":%s,"schedule_display":"%s"}' \
  "$n" "$name" "$2" "$en" "$sch" "$disp" > "$d/j/$n.json"
{ printf '{"jobs":['; first=1; for f in "$d"/j/*.json; do [ $first = 1 ] || printf ','; first=0; cat "$f"; done
  printf ']}'; } > "$d/jobs.json"
"#,
    );
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A spec job written as `every 2h` or a phrase is compared in Hermes' stored form: apply, plan, apply, plan
/// leaves it in sync and the second apply edits nothing.
#[test]
fn spec_cron_in_a_non_canonical_form_settles_after_one_apply() {
    let t = Tmp::new("spec-cron-settles");
    let acme = t.0.join("acme");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/acme");
    assert!(Command::new("cp")
        .arg("-R")
        .arg(&src)
        .arg(&acme)
        .status()
        .unwrap()
        .success());
    let cron = acme.join("template/cron.yaml");
    let y = fs::read_to_string(&cron)
        .unwrap()
        .replace("\"0 9 * * 1-5\"", "every 2h")
        .replace("\"0 16 * * 5\"", "every friday 16:00");
    w(&cron, &y);
    let hb = storing_hermes(&t.0);
    let hh = t.0.join("hermes-home");
    let go = |cmd: &str| {
        let o = bin()
            .args([cmd, "--bundle", s(&acme.join("grok")), "--spec"])
            .arg(acme.join("botport.toml"))
            .arg("--hermes-home")
            .arg(&hh)
            .arg("--hermes-bin")
            .arg(&hb)
            .arg("--json")
            .env_remove("HERMES_HOME")
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)
    };
    let cron_items = |out: &str| -> Vec<(String, String)> {
        let v: serde_json::Value = serde_json::from_str(out).unwrap_or_else(|_| panic!("{out}"));
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["area"] == "cron" && i["name"].as_str().unwrap().starts_with("acme-"))
            .map(|i| {
                (
                    i["name"].as_str().unwrap().into(),
                    i["status"].as_str().unwrap().into(),
                )
            })
            .collect()
    };
    for round in 0..2 {
        go("apply");
        let items = cron_items(&go("plan"));
        assert_eq!(items.len(), 2, "round {round}: {items:?}");
        assert!(
            items.iter().all(|(_, st)| st == "in-sync"),
            "round {round}: {items:?}"
        );
    }
    let log = fs::read_to_string(hh.join("profiles/acme-helper/cron/calls.log")).unwrap();
    assert!(
        log.contains("every 120m") && log.contains("0 16 * * 5"),
        "{log}"
    );
    assert!(!log.contains("edit "), "{log}");
}

/// `botport ARGS` with `input` on stdin and its own state dir.
fn run_stdin(t: &Path, args: &[&str], input: &str) -> (i32, String) {
    use std::io::Write;
    let mut c = bin();
    c.env("XDG_STATE_HOME", t)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut ch = c.spawn().unwrap();
    ch.stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let o = ch.wait_with_output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
    )
}

#[test]
fn pasted_export_on_stdin_is_repaired_reported_and_verified() {
    let t = Tmp::new("paste");
    let exp = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grok-export/grok-bot-export.json");
    let doc = fs::read_to_string(&exp).unwrap();
    let (rc, strict) = run(&["verify", "--bundle", s(&exp)]);
    assert_eq!(rc, 0, "{strict}");
    assert!(!strict.contains("repaired"), "{strict}");
    // a chat reply: chatter, a fenced block, a trailing comma
    let pasted = format!(
        "Here is my export:\n```json\n{}\n```\nAnything else?\n",
        doc.replacen("\n  ]\n}", "\n  ],\n}", 1)
    );
    let (rc, out) = run_stdin(&t.0, &["verify", "--bundle", "-"], &pasted);
    assert_eq!(rc, 0, "{out}");
    assert!(
        out.contains("repaired export: took the JSON out of a fenced code block"),
        "{out}"
    );
    assert!(
        out.contains("repaired export: removed 1 trailing commas"),
        "{out}"
    );
    let first = |o: &str| {
        o.lines()
            .find(|l| l.starts_with("verify OK"))
            .map(String::from)
    };
    assert_eq!(first(&out), first(&strict));
    // saved for this run only, in a per-run dir that is gone afterwards
    assert!(
        out.contains("/botport/paste-") && out.contains("for this run"),
        "{out}"
    );
    let left: Vec<_> = fs::read_dir(t.0.join("botport"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("paste"))
        .collect();
    assert!(left.is_empty(), "{left:?}");

    // the secret guard runs on the repaired text: nothing is saved
    let t2 = Tmp::new("paste-secret");
    let bad = pasted.replacen(
        "\"name\": \"Nova\"",
        "\"name\": \"Nova\", \"password\": \"x\"",
        1,
    );
    let (rc, out) = run_stdin(&t2.0, &["verify", "--bundle", "-"], &bad);
    assert_eq!(rc, 2, "{out}");
    assert!(
        out.contains("credential-looking keys") && out.contains("password"),
        "{out}"
    );
    assert!(fs::read_dir(t2.0.join("botport")).map_or(true, |d| d
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .all(|n| !n.starts_with("paste"))));
}

#[test]
fn pasted_export_never_picks_up_a_describe_json_in_the_state_dir() {
    let t = Tmp::new("paste-describe");
    let exp = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grok-export/grok-bot-export.json");
    let doc = fs::read_to_string(&exp).unwrap();
    let (_, plain) = run_stdin(&t.0, &["verify", "--bundle", "-"], &doc);
    fs::create_dir_all(t.0.join("botport")).unwrap();
    fs::write(
        t.0.join("botport/describe.json"),
        r#"{"persona": "PLANTED-DESCRIPTION"}"#,
    )
    .unwrap();
    let (rc, out) = run_stdin(&t.0, &["verify", "--bundle", "-"], &doc);
    assert_eq!(rc, 0, "{out}");
    let first = |o: &str| {
        o.lines()
            .find(|l| l.starts_with("verify OK"))
            .map(String::from)
    };
    assert_eq!(first(&out), first(&plain));
    assert!(!out.contains("PLANTED"), "{out}");
}

#[test]
fn paste_input_conflicts_are_refused() {
    let t = Tmp::new("paste-args");
    for (args, want) in [
        (
            &["verify", "--bundle", "x", "--clipboard"][..],
            "--bundle and --clipboard",
        ),
        (
            &["verify", "--bundle", "-", "--clipboard"][..],
            "--bundle - and --clipboard",
        ),
        (
            &["verify", "--bundle", "-", "--bundle", "x"][..],
            "more than once",
        ),
        (
            &["verify", "--bundle", "x", "--bundle", "-"][..],
            "more than once",
        ),
        (
            &["verify", "--bundle", "x", "--bundle", "y", "--clipboard"][..],
            "more than once",
        ),
    ] {
        let (rc, out) = run_stdin(&t.0, args, "{}");
        assert_eq!(rc, 2, "{args:?}: {out}");
        assert!(out.contains(want), "{args:?}: {out}");
    }
}

#[test]
fn clipboard_without_a_clipboard_tool_is_a_clear_error() {
    let t = Tmp::new("clip");
    let o = bin()
        .env("XDG_STATE_HOME", &t.0)
        .env("PATH", &t.0)
        .args(["verify", "--clipboard"])
        .output()
        .unwrap();
    let e = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{e}");
    assert!(
        e.contains("--clipboard needs") && e.contains("--bundle -"),
        "{e}"
    );
}

#[test]
fn smart_quoted_paste_is_refused_with_plain_advice() {
    let t = Tmp::new("paste-smart");
    let pasted = "```json\n{“format”: “grok-bot-export”, “bots”: [{“name”: “Nova”, “memories”: [“Use “,” here”]}]}\n```\n";
    let (rc, out) = run_stdin(&t.0, &["verify", "--bundle", "-"], pasted);
    assert_eq!(rc, 2, "{out}");
    assert!(
        out.contains("stdin: the export was mangled by the chat app (smart quotes); ask the bot to re-send it as a file or a plain code block"),
        "{out}"
    );
    assert!(!out.contains("not valid JSON"), "{out}");
}

/// The audits' token cases (rounds 2-3): a token whole, split across fragments at any point, behind a `\u`
/// escape or in a smart-quoted paste is refused and nothing is saved.
#[test]
fn token_split_across_pasted_fragments_is_refused_once_joined() {
    let xai = "xai-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U1v2W3x4Y5z6";
    let ant = format!(
        "sk-ant-api03-{}-AbCdEf",
        "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789".repeat(2)
    );
    let ghp = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let doc = |m: &str, q: char| {
        format!(r#"{{"format": "grok-bot-export", "version": 1, "bots": [{{"name": "Nova", "memories": ["k {m}"]}}]}}"#)
            .replace('"', &q.to_string())
    };
    let split = |m: &str, at: usize| {
        let d = doc(m, '"');
        let (a, b) = d.split_at(d.find(m).unwrap() + at);
        format!("```json\n{a}\n```\n(more)\n```json\n{b}\n```\n")
    };
    let cases = [
        ("22", split(xai, 20)),
        ("23", format!("```json\n{}\n```\n", doc(xai, '"'))),
        ("25", doc(xai, '“')),
        ("26", split(xai, 7)),
        ("57", split(xai, 5)),
        ("58", split(xai, 2)),
        ("60", doc(&xai.replacen('x', "\\u0078", 1), '"')),
        ("61", split(&ant, 14)),
        ("62", split(&ghp, 4)),
        ("short", {
            let d = r#"{"format": "grok-bot-export", "version": 1, "bots": [{"name": "Nova", "memories": ["key xai-ABCDEFGHIJKLMNOPQRSTUVWX", "ok"]}]}"#;
            let (a, b) = d.split_at(d.find("xai-ABC").unwrap() + 7);
            format!("```json\n{a}\n```\n(more)\n```\n{b}\n```\n")
        }),
    ];
    for (n, pasted) in cases {
        let t = Tmp::new(&format!("paste-token-{n}"));
        let (rc, out) = run_stdin(&t.0, &["verify", "--bundle", "-"], &pasted);
        assert_eq!(rc, 2, "{n}: {out}");
        assert!(
            fs::read_dir(t.0.join("botport")).map_or(true, |d| d
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .all(|n| !n.starts_with("paste"))),
            "{n}"
        );
    }
}

/// The plan/drift/apply args for the fixture into `<t>/hermes` (profile demo).
fn demo_args(t: &Path) -> (PathBuf, Vec<String>) {
    let (g, spec) = fixture(t);
    let hh = t.join("hermes");
    let v = [
        "--bundle",
        s(&g),
        "--spec",
        s(&spec),
        "--hermes-home",
        s(&hh),
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    (hh.join("profiles/demo"), v)
}

fn run_with(cmd: &str, base: &[String], extra: &[&str]) -> (i32, String) {
    let mut v = vec![cmd];
    v.extend(base.iter().map(String::as_str));
    v.extend(extra);
    run(&v)
}

/// Hold the profile's operation lock from this (test) process, as another botport run would: flock is per open
/// file, so the child binary sees it held.
fn hold_lock(prof: &Path) -> fs::File {
    fs::create_dir_all(prof).unwrap();
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(prof.join(".botport.lock"))
        .unwrap();
    f.try_lock().unwrap();
    use std::io::Write;
    writeln!(&f, "pid=4242 since=1").unwrap();
    f
}

#[test]
fn apply_refuses_a_held_lock_and_runs_once_it_is_released() {
    let t = Tmp::new("oplock");
    let (prof, base) = demo_args(&t.0);
    let lock = prof.join(".botport.lock");
    let holder = hold_lock(&prof);
    for cmd in ["apply", "sync"] {
        let (rc, out) = run_with(cmd, &base, &[]);
        assert_eq!(rc, 2, "{out}");
        assert!(
            out.contains("another botport run holds") && out.contains("pid=4242"),
            "{out}"
        );
    }
    assert!(!prof.join("backups").exists() && !prof.join("skills").exists());
    // plan and drift stay lock-free: they run and leave the holder's lock alone
    assert_eq!(run_with("drift", &base, &[]).0, 1);
    assert_eq!(run_with("plan", &base, &[]).0, 0);
    drop(holder); // released, as the kernel does when a run exits, however it exits
    let (rc, out) = run_with("apply", &base, &[]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        lock.is_file(),
        "the lock file stays; only the lock is released"
    );
    let held = fs::read_to_string(&lock).unwrap();
    assert!(held.starts_with("pid=") && !held.contains("4242"), "{held}");
    assert_eq!(run_with("drift", &base, &[]).0, 0);
}

#[test]
fn a_symlinked_or_odd_lock_file_is_refused() {
    let t = Tmp::new("oplock-link");
    let (prof, base) = demo_args(&t.0);
    fs::create_dir_all(&prof).unwrap();
    let target = t.0.join("elsewhere");
    w(&target, "x");
    std::os::unix::fs::symlink(&target, prof.join(".botport.lock")).unwrap();
    let (rc, out) = run_with("apply", &base, &[]);
    assert_eq!(rc, 2, "{out}");
    assert!(out.contains("is a symlink"), "{out}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "x");
    fs::remove_file(prof.join(".botport.lock")).unwrap();
    fs::create_dir(prof.join(".botport.lock")).unwrap();
    let (rc, out) = run_with("apply", &base, &[]);
    assert_eq!(rc, 2, "{out}");
    assert!(out.contains("not a regular file"), "{out}");
    assert!(!prof.join("skills").exists());
}

#[test]
fn a_held_lock_refuses_the_h2g_writers() {
    let t = Tmp::new("oplock-h2g");
    let (prof, base) = demo_args(&t.0);
    assert_eq!(run_with("apply", &base, &[]).0, 0);
    w(
        &prof.join("cron/jobs.json"),
        r#"{"jobs":[{"name":"hermes-own","schedule":"0 8 * * *","prompt":"hi","deliver":"local"}]}"#,
    );
    let mem = prof.join("memories/MEMORY.md");
    let m0 = fs::read_to_string(&mem).unwrap_or_default();
    w(&mem, &format!("{m0}A hermes fact.\n"));
    let rec = prof.join("botport.lock.json");
    let before = fs::read_to_string(&rec).unwrap();
    let out_dir = t.0.join("h2g-out");
    let o = s(&out_dir);
    let holder = hold_lock(&prof);
    for args in [
        vec!["export-hermes", "--out", o],
        vec!["sync", "--direction", "h2g", "--out", o],
        vec!["sync", "--direction", "both", "--out", o],
    ] {
        let (rc, out) = run_with(args[0], &base, &args[1..]);
        assert_eq!(rc, 2, "{args:?}: {out}");
        assert!(out.contains("another botport run holds"), "{out}");
        assert!(
            fs::read_to_string(&rec).unwrap() == before,
            "{args:?} touched the record"
        );
        assert!(!out_dir.exists(), "{args:?} wrote the bundle");
    }
    // read-only h2g stays lock-free
    assert_ne!(run_with("drift", &base, &["--direction", "h2g"]).0, 2);
    drop(holder);
    let (rc, out) = run_with("export-hermes", &base, &["--out", o]);
    assert_eq!(rc, 0, "{out}");
    assert!(
        fs::read_to_string(&rec).unwrap() != before,
        "the export records what it sent: {out}"
    );
}

#[test]
fn a_read_only_in_sync_profile_still_applies() {
    use std::os::unix::fs::PermissionsExt;
    let t = Tmp::new("oplock-ro");
    let (prof, base) = demo_args(&t.0);
    assert_eq!(run_with("apply", &base, &[]).0, 0);
    fs::remove_file(prof.join(".botport.lock")).unwrap();
    fs::set_permissions(&prof, fs::Permissions::from_mode(0o555)).unwrap();
    let r = run_with("apply", &base, &[]);
    let s2 = run_with("sync", &base, &["--direction", "g2h"]);
    fs::set_permissions(&prof, fs::Permissions::from_mode(0o755)).unwrap();
    if is_root() {
        return; // root ignores the mode
    }
    assert_eq!(r.0, 0, "{}", r.1);
    assert!(r.1.contains("read-only"), "{}", r.1);
    assert_eq!(s2.0, 0, "{}", s2.1);
    assert!(!prof.join(".botport.lock").exists());
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Many runs looping `apply` on one profile: a fake hermes marks the locked section (`mkdir inside`, held for
/// each `cron create`), so two runs inside it at once is an overlap. Workers/iterations: BOTPORT_STRESS_WORKERS
/// (default 16) and BOTPORT_STRESS_ITERS (default 6).
#[test]
fn concurrent_applies_never_overlap_under_load() {
    use std::os::unix::fs::PermissionsExt;
    let t = Tmp::new("oplock-stress");
    let race = t.0.join("race");
    fs::create_dir_all(&race).unwrap();
    let fake = t.0.join("fake-hermes");
    w(
        &fake,
        &format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'Hermes Agent v0.22.5'; exit 0; }}\n\
             if mkdir '{r}/inside' 2>/dev/null; then sleep 0.02; rmdir '{r}/inside'; \
             else echo overlap >> '{r}/overlaps'; sleep 0.02; fi\necho \"$*\" >> '{r}/calls'\n",
            r = race.display()
        ),
    );
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let env_n = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (workers, iters) = (
        env_n("BOTPORT_STRESS_WORKERS", 16),
        env_n("BOTPORT_STRESS_ITERS", 6),
    );
    let acme = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/acme");
    let hh = t.0.join("hh");
    let ths: Vec<_> = (0..workers)
        .map(|n| {
            let (acme, hh, fake, d) = (
                acme.clone(),
                hh.clone(),
                fake.clone(),
                t.0.join(format!("d{n}")),
            );
            std::thread::spawn(move || {
                let mut res = vec![];
                for _ in 0..iters {
                    let o = bin()
                        .args(["apply", "--bundle", s(&acme.join("grok")), "--spec"])
                        .arg(acme.join("botport.toml"))
                        .arg("--hermes-home")
                        .arg(&hh)
                        .arg("--hermes-bin")
                        .arg(&fake)
                        .arg("--details")
                        .arg(&d)
                        .env_remove("HERMES_HOME")
                        .output()
                        .unwrap();
                    res.push((
                        o.status.code(),
                        String::from_utf8_lossy(&o.stdout).to_string()
                            + &String::from_utf8_lossy(&o.stderr),
                    ));
                }
                res
            })
        })
        .collect();
    let (mut ok, mut refused) = (0, 0);
    for th in ths {
        for (rc, out) in th.join().unwrap() {
            match rc {
                Some(0) => ok += 1,
                Some(2) if out.contains("another botport run holds") => refused += 1,
                _ => panic!("unexpected result {rc:?}: {out}"),
            }
        }
    }
    let overlaps = fs::read_to_string(race.join("overlaps")).unwrap_or_default();
    let calls = fs::read_to_string(race.join("calls")).unwrap_or_default();
    eprintln!(
        "stress {workers}x{iters}: {ok} ok, {refused} refused, {} hermes calls, {} overlaps",
        calls.lines().count(),
        overlaps.lines().count()
    );
    assert!(overlaps.is_empty(), "two runs were inside the lock at once");
    assert!(ok >= 1 && calls.lines().count() >= 3);
}

#[test]
fn malformed_jobs_json_is_an_error_not_no_jobs() {
    for bad in ["{\"jobs\": [", "{\"jobs\": 3}", "\"x\""] {
        let t = Tmp::new("badjobs");
        let (prof, base) = demo_args(&t.0);
        w(&prof.join("cron/jobs.json"), bad);
        for cmd in ["apply", "sync", "drift"] {
            let (rc, out) = run_with(cmd, &base, &[]);
            assert_eq!(rc, 2, "{cmd} {bad}: {out}");
            assert!(out.contains("cron/jobs.json: malformed"), "{out}");
            assert!(out.contains("nothing was written"), "{out}");
        }
        assert!(!prof.join("backups").exists() && !prof.join("skills").exists());
        assert_eq!(
            fs::read_to_string(prof.join("cron/jobs.json")).unwrap(),
            bad
        );
    }
}

#[test]
fn unwritable_details_report_warns_once_and_keeps_the_exit_code() {
    let t = Tmp::new("details");
    let (_, base) = demo_args(&t.0);
    w(&t.0.join("file"), "x");
    let bad = t.0.join("file/report.json");
    let (rc_good, _) = run_with("plan", &base, &[]);
    let o = bin()
        .arg("plan")
        .args(&base)
        .args(["--details", s(&bad)])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code().unwrap(), rc_good, "{err}");
    assert_eq!(
        err.matches("could not write the details report").count(),
        1,
        "{err}"
    );
    assert!(err.contains(&bad.display().to_string()), "{err}");
    assert!(!String::from_utf8_lossy(&o.stdout).contains("details:"));
}

#[cfg(unix)]
#[test]
fn clear_out_never_follows_a_symlink() {
    // A tampered old export lists link/victim while `link` points outside the export dir: the victim must survive.
    let t = Tmp::new("clrlink");
    let (g, _) = fixture(&t.0);
    let outside = t.0.join("outside");
    w(&outside.join("victim"), "keep me\n");
    let out = t.0.join("b");
    fs::create_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(&outside, out.join("link")).unwrap();
    w(
        &out.join("manifest.json"),
        r#"{"tool":"botport","files":{"link/victim":"x"}}"#,
    );
    let (rc, o) = run(&["export", "--from-dir", s(&g), "--out", s(&out)]);
    assert_eq!(
        fs::read_to_string(outside.join("victim")).unwrap(),
        "keep me\n"
    );
    assert_ne!(rc, 0, "{o}");
    assert!(o.contains("link/victim"), "skipped path reported: {o}");
}

#[cfg(unix)]
#[test]
fn bundle_is_private_and_history_is_opt_in() {
    use std::os::unix::fs::PermissionsExt;
    let t = Tmp::new("priv");
    let (g, _) = fixture(&t.0);
    let h = t.0.join("chat.sqlite");
    w(&h, "SQLite format 3\0 private chat\n");
    let describe = t.0.join("d.md");
    w(&describe, "## Instructions\nBe kind.\n");
    // default: no history, even when a history file is named; an explicit history path without the flag is refused
    let out = t.0.join("b");
    let (rc, o) = run(&[
        "export",
        "--from-dir",
        s(&g),
        "--describe",
        s(&describe),
        "--out",
        s(&out),
    ]);
    assert_eq!(rc, 0, "{o}");
    assert!(o.contains("history off"), "{o}");
    assert!(!out.join("history").exists());
    let (rc, o) = run(&[
        "export",
        "--from-dir",
        s(&g),
        "--history-sqlite",
        s(&h),
        "--out",
        s(&t.0.join("b2")),
    ]);
    assert_eq!(rc, 2, "{o}");
    assert!(o.contains("--with-history"), "{o}");
    // opt in: history is written, and every bundle file is 0600 in 0700 dirs
    let (rc, o) = run(&[
        "export",
        "--from-dir",
        s(&g),
        "--describe",
        s(&describe),
        "--with-history",
        "--history-sqlite",
        s(&h),
        "--out",
        s(&out),
    ]);
    assert_eq!(rc, 0, "{o}");
    assert!(out.join("history/chat.sqlite").is_file(), "{o}");
    fn walk(d: &Path, n: &mut usize) {
        let m = fs::metadata(d).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o700, "{}", d.display());
        for e in fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n);
            } else {
                *n += 1;
                let m = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(m, 0o600, "{}", p.display());
            }
        }
    }
    let mut n = 0;
    walk(&out, &mut n);
    assert!(n >= 6, "{n}");
    // verify still accepts the private bundle
    let (rc, o) = run(&["verify", "--bundle", s(&out)]);
    assert_eq!(rc, 0, "{o}");
}

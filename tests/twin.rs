//! End-to-end tests of `botport grok list` and `botport twin ...` on the invented demo pair (no Grok account, no
//! hermes CLI, a throwaway Hermes home).
mod common;
use common::Tmp;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

fn run(t: &Path, args: &[&str]) -> (i32, Value) {
    let home = t.join("home");
    let state = t.join("state");
    let o = Command::new(env!("CARGO_BIN_EXE_botport"))
        .args(args)
        .args([
            "--hermes-home",
            home.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "--json",
        ])
        .env("XDG_STATE_HOME", t.join("xdg-state"))
        .env("XDG_CONFIG_HOME", t.join("xdg-config"))
        .env("HERMES_GATEWAY_LOCK_DIR", t.join("gateway-locks"))
        // never this machine's real home, state or config folders
        .env("HOME", t.join("home-dir"))
        .env_remove("HERMES_HOME")
        .output()
        .unwrap();
    let out = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        out.matches('\n').count(),
        1,
        "exactly one JSON line on stdout: {out}"
    );
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"));
    assert_eq!(v["schema"], "botport.twin.v1");
    // no absolute path of this machine in any output
    assert!(
        !out.contains(t.to_str().unwrap()),
        "absolute path in output: {out}"
    );
    (o.status.code().unwrap(), v)
}

fn data(v: &Value) -> &Value {
    assert_eq!(v["ok"], true, "{v}");
    &v["data"]
}

fn pair<'a>(v: &'a Value, id: &str) -> &'a Value {
    data(v)["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pair_id"] == id)
        .unwrap()
}

#[test]
fn version_lists_features() {
    let t = Tmp::new("twin-version");
    let (code, v) = run(&t.0, &["version"]);
    assert_eq!(code, 0);
    assert!(data(&v)["features"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f == "twin-sync"));
}

#[test]
fn errors_are_json_too() {
    let t = Tmp::new("twin-errors");
    let (code, v) = run(&t.0, &["twin", "status", "--pair", "nope"]);
    assert_eq!(code, 3);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "pair_not_found");
    // a path the caller gave is never echoed back
    let (code, v) = run(
        &t.0,
        &[
            "twin",
            "create",
            "--all",
            "--from-export",
            "/nonexistent/folder-name-x1/export.json",
            "--grok-app-dir",
            "/nonexistent/app-dir-y2",
        ],
    );
    assert!(
        !v.to_string().contains("folder-name-x1") && !v.to_string().contains("app-dir-y2"),
        "{v}"
    );
    assert!(code == 0 || code >= 2, "{v}");
    let (code, v) = run(&t.0, &["twin", "bogus"]);
    assert_eq!((code, v["error"]["code"].as_str()), (2, Some("usage")));
}

#[test]
fn demo_flow_create_rename_secret_conflict_restore() {
    let t = Tmp::new("twin-demo");
    let (code, v) = run(&t.0, &["twin", "demo"]);
    assert_eq!(code, 0, "{v}");
    let acme = pair(&v, "acme-helper");
    assert_eq!(acme["state"], "in_step");
    assert_eq!(acme["hermes"]["created"], true);
    let soul = fs::read_to_string(t.0.join("home/profiles/acme-helper/SOUL.md")).unwrap();
    assert!(soul.contains("Acme Helper") && soul.contains("You are Acme Helper"));
    assert!(t
        .0
        .join("home/profiles/acme-helper/skills/grok-twin/greet-visitors/SKILL.md")
        .is_file());

    // the cache lists both invented bots, twinned; --only filters by name
    let (_, v) = run(&t.0, &["grok", "list", "--only", "acme*"]);
    let bots = data(&v)["bots"].as_array().unwrap();
    assert_eq!(bots.len(), 1);
    assert_eq!(bots[0]["twinned_pair_id"], "acme-helper");
    assert_eq!(bots[0]["routines"][0], "Morning summary");

    // setup parity: every item in step; the routine is not recreated in Hermes yet, and the demo twin has no model
    let (_, v) = run(&t.0, &["twin", "parity", "--pair", "acme-helper"]);
    let d = data(&v);
    assert_eq!(d["kind"], "setup");
    let failed: Vec<&str> = d["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["pass"] == false)
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(failed, ["routine:Morning summary", "model"], "{d}");

    // nothing changed: check says so, without reading content
    let (_, v) = run(&t.0, &["twin", "check"]);
    assert_eq!(data(&v)["changed"].as_array().unwrap().len(), 0);

    // a rename in Grok: check sees it, sync carries name and description
    run(&t.0, &["twin", "demo", "--step", "rename"]);
    let soul = fs::read_to_string(t.0.join("home/profiles/acme-helper/SOUL.md")).unwrap();
    assert!(soul.contains("Acme Support Desk"));
    let (_, v) = run(&t.0, &["twin", "recent", "--pair", "acme-helper"]);
    assert!(data(&v)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "name_change"));

    // a key-shaped value in the description is held back whole, its location reported, never its value
    let (code, v) = run(&t.0, &["twin", "demo", "--step", "secret"]);
    assert_eq!(code, 1);
    let acme = pair(&v, "acme-helper");
    assert_eq!(acme["state"], "needs_you");
    assert_eq!(acme["secrets_withheld"][0]["item"], "persona.description");
    assert!(!v.to_string().contains("NOTAREALKEY"));
    let soul = fs::read_to_string(t.0.join("home/profiles/acme-helper/SOUL.md")).unwrap();
    assert!(!soul.contains("NOTAREALKEY"));
    let (code, _) = run(&t.0, &["twin", "demo", "--step", "reset"]);
    assert_eq!(code, 0);

    // the same skill changed on both sides: a conflict with both copies and a diff
    let (code, v) = run(&t.0, &["twin", "demo", "--step", "conflict"]);
    assert_eq!(code, 1);
    assert_eq!(
        pair(&v, "acme-helper")["conflicts"][0],
        "skill:greet-visitors"
    );
    let (_, v) = run(&t.0, &["twin", "conflicts", "--pair", "acme-helper"]);
    let c = &data(&v)["conflicts"][0];
    assert!(c["diff"].as_str().unwrap().contains("+Always end with"));
    assert!(c["grok"]["text"]
        .as_str()
        .unwrap()
        .contains("widget colours"));

    // keep the Hermes copy: the conflict clears and the change is listed to apply in Grok by hand
    let (code, _) = run(
        &t.0,
        &[
            "twin",
            "resolve",
            "--pair",
            "acme-helper",
            "--conflict",
            "skill:greet-visitors",
            "--keep",
            "hermes",
        ],
    );
    assert_eq!(code, 0);
    let (_, v) = run(&t.0, &["twin", "status", "--pair", "acme-helper"]);
    let s = pair(&v, "acme-helper");
    assert_eq!(s["conflicts"], 0);
    assert_eq!(s["state"], "changes_waiting");
    assert!(s["by_hand"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["item"] == "skill:greet-visitors" && b["side"] == "grok"));

    // restore an older name from its kept copy
    let (_, v) = run(&t.0, &["twin", "restore-points", "--pair", "acme-helper"]);
    let old = data(&v)["points"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|p| p["item"] == "persona.name")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, _) = run(
        &t.0,
        &["twin", "restore", "--pair", "acme-helper", "--point", &old],
    );
    assert_eq!(code, 0);

    // the fake key is nowhere outside the fake Grok app folder: not in a twin file, snapshot, kept copy or restore point
    assert_absent(&t.0, "NOTAREALKEY", &t.0.join("state/demo-grok-app"));

    // botport never used the user-wide config or state folders
    assert!(!t.0.join("xdg-config").exists() && !t.0.join("xdg-state").exists());

    // removing the twin removes its files and restore points; the profile stays
    let (code, v) = run(&t.0, &["twin", "remove", "--pair", "acme-helper"]);
    assert_eq!(code, 0, "{v}");
    assert!(!t.0.join("state/pairs/acme-helper.key").exists());
    assert!(!t
        .0
        .join("home/profiles/acme-helper/backups/botport")
        .exists());
    assert!(!t.0.join("home/profiles/acme-helper/botport").exists());
    assert!(t.0.join("home/profiles/acme-helper/SOUL.md").is_file());
    let (_, v) = run(&t.0, &["twin", "status", "--all"]);
    assert_eq!(data(&v)["pairs"].as_array().unwrap().len(), 1);
}

/// No file under `root` (zip archives included, read as bytes) holds `needle`, except below `except`.
fn assert_absent(root: &Path, needle: &str, except: &Path) {
    for e in fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        if p.starts_with(except) || e.file_type().unwrap().is_symlink() {
            continue;
        }
        if p.is_dir() {
            assert_absent(&p, needle, except);
        } else {
            let b = fs::read(&p).unwrap();
            assert!(
                !b.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{} holds the fake key",
                p.display()
            );
        }
    }
}

#[test]
fn an_existing_profile_is_never_taken_over_by_create() {
    let t = Tmp::new("twin-collide");
    run(&t.0, &["twin", "demo"]);
    let app = t.0.join("state/demo-grok-app");
    let t2 = Tmp::new("twin-collide2");
    // the owner already has a profile named like the bot
    fs::create_dir_all(t2.0.join("home/profiles/acme-helper")).unwrap();
    fs::write(
        t2.0.join("home/profiles/acme-helper/SOUL.md"),
        "Owner's own profile.\n",
    )
    .unwrap();
    let (code, v) = run(
        &t2.0,
        &[
            "twin",
            "create",
            "--grok-id",
            "b-acme-0001",
            "--grok-app-dir",
            app.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(data(&v)["pairs"][0]["hermes"]["profile"], "acme-helper-2");
    assert_eq!(
        fs::read_to_string(t2.0.join("home/profiles/acme-helper/SOUL.md")).unwrap(),
        "Owner's own profile.\n"
    );
}

#[test]
fn remove_refuses_a_tampered_index() {
    let t = Tmp::new("twin-tamper");
    run(&t.0, &["twin", "demo"]);
    let p = t.0.join("state/twin.json");
    let s = fs::read_to_string(&p).unwrap().replace(
        "\"profile\": \"acme-helper\"",
        "\"profile\": \"../../outside\"",
    );
    fs::write(&p, s).unwrap();
    let (code, v) = run(&t.0, &["twin", "remove", "--pair", "acme-helper"]);
    assert_eq!(code, 4, "{v}");
    assert!(t.0.join("state/pairs/acme-helper.key").exists());
}

#[test]
fn create_preview_writes_nothing() {
    let t = Tmp::new("twin-preview");
    // the demo's fake app folder, but no twins yet: set it up, then preview another state folder's create
    run(&t.0, &["twin", "demo"]);
    let app = t.0.join("state/demo-grok-app");
    let t2 = Tmp::new("twin-preview2");
    let (code, v) = run(
        &t2.0,
        &[
            "twin",
            "create",
            "--all",
            "--dry-run",
            "--grok-app-dir",
            app.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(data(&v)["pairs"].as_array().unwrap().len(), 2);
    assert!(!t2.0.join("home").exists());
    assert!(!t2.0.join("state/twin.json").exists());
}

fn botport(t: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_botport"))
        .args(args)
        .env("XDG_STATE_HOME", t.join("box-xdg-state"))
        .env("XDG_CONFIG_HOME", t.join("box-xdg-config"))
        .env("HOME", t.join("box-home"))
        .output()
        .unwrap()
}

/// Copy every file under `from` to the same path under `to` (the Mirror bot's downloads and uploads, by path).
fn copy_tree(from: &Path, to: &Path) {
    for e in fs::read_dir(from).unwrap().flatten() {
        let (s, d) = (e.path(), to.join(e.file_name()));
        if s.is_dir() {
            fs::create_dir_all(&d).unwrap();
            copy_tree(&s, &d);
        } else {
            fs::copy(&s, &d).unwrap();
        }
    }
}

#[test]
fn mirror_bot_carries_a_hermes_skill_to_grok_and_only_the_pairs_skills_come_back() {
    let t = Tmp::new("twin-mirror-bot");
    run(&t.0, &["twin", "demo"]);
    // the pair folder the owner's sync app shares with the Mirror bot (outside the Hermes home)
    let drive = t.0.join("drive/acme-pair");
    fs::create_dir_all(&drive).unwrap();
    let (code, v) = run(
        &t.0,
        &[
            "twin",
            "mirror",
            "--pair",
            "acme-helper",
            "--pair-folder",
            drive.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert!(data(&v)["charter"].as_str().unwrap().contains("Mirror bot"));

    // the Hermes twin learns a skill; after the owner's approval it is published for the Grok side
    let sk =
        t.0.join("home/profiles/acme-helper/skills/grok-twin/greet-the-team");
    fs::create_dir_all(&sk).unwrap();
    fs::write(sk.join("SKILL.md"), "---\nname: greet-the-team\ndescription: \"Say hello to the whole team\"\n---\n\n# greet-the-team\n\nGreet each team member by name, then wish them a good day.\n").unwrap();
    let (_, v) = run(&t.0, &["twin", "sync", "--pair", "acme-helper"]);
    assert_eq!(
        pair(&v, "acme-helper")["waiting"][0]["item"],
        "skill:greet-the-team"
    );
    let (_, v) = run(
        &t.0,
        &[
            "twin",
            "approve",
            "--pair",
            "acme-helper",
            "--item",
            "skill:greet-the-team",
        ],
    );
    assert_eq!(data(&v)["sync"]["to_grok"], 1, "{v}");
    let (_, v) = run(&t.0, &["twin", "status", "--pair", "acme-helper"]);
    assert_eq!(pair(&v, "acme-helper")["state"], "changes_waiting");
    assert!(drive.join("HEAD").is_file());

    // the Mirror bot's run inside the Grok box, on an invented box: another bot's skill sits in the shared skills
    let root = t.0.join("box/botport-mirror");
    // as in a real box, the agent data folder is itself a link to the real folder
    let agent_data = t.0.join("box/agent-data");
    let real_data = t.0.join("box/agent-data-real");
    fs::create_dir_all(&real_data).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_data, &agent_data).unwrap();
    #[cfg(not(unix))]
    fs::create_dir_all(&agent_data).unwrap();
    let id = "00000000-0000-4000-8000-00000000ac3e";
    fs::create_dir_all(agent_data.join("agents").join(id)).unwrap();
    fs::write(
        agent_data.join("agents").join(id).join("profile.json"),
        r#"{"name": "Acme Helper"}"#,
    )
    .unwrap();
    fs::create_dir_all(agent_data.join("workflows/other-bots-skill")).unwrap();
    fs::write(
        agent_data.join("workflows/other-bots-skill/SKILL.md"),
        "---\nname: other-bots-skill\n---\nNot this twin's.\n",
    )
    .unwrap();
    let work = root.join(".work/acme-helper");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("agent-id"), id).unwrap();
    fs::write(work.join("pair-skills"), "").unwrap();
    fs::copy(
        t.0.join("state/pairs/acme-helper.key"),
        work.join("pair.key"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(work.join("pair.key"), fs::Permissions::from_mode(0o600)).unwrap();
    }
    let local = root.join("acme-helper");
    fs::create_dir_all(&local).unwrap();
    copy_tree(&drive, &local);
    let (r, ad) = (root.to_str().unwrap(), agent_data.to_str().unwrap());
    let o = botport(
        &t.0,
        &["mirror", "apply-pack", "--root", r, "--agent-data", ad],
    );
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(
        out.contains("install") && out.contains("greet-the-team"),
        "{out}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let install = |to: &Path| {
        botport(
            &t.0,
            &[
                "mirror",
                "apply-pack",
                "--root",
                r,
                "--agent-data",
                ad,
                "--pair",
                "acme-helper",
                "--precheck",
                "skill:greet-the-team",
                "--gen",
                "1",
                "--install-to",
                to.to_str().unwrap(),
            ],
        )
    };
    // anywhere but the box's skills folder is refused, and so is a skills folder that is a link
    let elsewhere = t.0.join("box/elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let o = install(&elsewhere);
    assert!(!o.status.success());
    assert!(!elsewhere.join("greet-the-team").exists());
    #[cfg(unix)]
    {
        let skills = real_data.join("workflows");
        let kept = real_data.join("workflows-kept");
        fs::rename(&skills, &kept).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &skills).unwrap();
        let o = install(&agent_data.join("workflows"));
        let e = String::from_utf8_lossy(&o.stdout).to_string();
        assert!(!o.status.success() && e.contains("is a link"), "{e}");
        assert!(!elsewhere.join("greet-the-team").exists());
        fs::remove_file(&skills).unwrap();
        fs::rename(&kept, &skills).unwrap();
    }
    // the resolved spelling is accepted as well as the link's
    let o = install(&real_data.join("workflows"));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let installed = real_data.join("workflows/greet-the-team");
    assert!(installed.join("SKILL.md").is_file());
    fs::remove_dir_all(&installed).unwrap();
    let o = install(&agent_data.join("workflows"));
    // botport installs the copy it verified (no separate copy step for the bot to race)
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(agent_data
        .join("workflows/greet-the-team/SKILL.md")
        .is_file());
    let o = botport(
        &t.0,
        &[
            "mirror",
            "apply-pack",
            "--root",
            r,
            "--agent-data",
            ad,
            "--check",
        ],
    );
    assert!(
        o.status.success(),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    let o = botport(
        &t.0,
        &["mirror", "digest-items", "--root", r, "--agent-data", ad],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    fs::create_dir_all(drive.join("from-grok")).unwrap();
    copy_tree(&local.join("from-grok"), &drive.join("from-grok"));

    // back on the owner's machine: the install is seen, the twin is in step, and the other bot's skill never arrives
    let (_, v) = run(&t.0, &["twin", "sync", "--pair", "acme-helper"]);
    assert_eq!(pair(&v, "acme-helper")["state"], "in_step", "{v}");
    let (_, v) = run(&t.0, &["twin", "status", "--pair", "acme-helper"]);
    let s = pair(&v, "acme-helper");
    assert_eq!(s["state"], "in_step", "{s}");
    assert_eq!(s["pending"]["in_flight"], 0);
    assert!(!t
        .0
        .join("home/profiles/acme-helper/skills/grok-twin/other-bots-skill")
        .exists());
    // disconnect (the key is replaced) and connect again: each transport keeps its own sequence floor, so neither
    // switch breaks a sync
    let key0 = fs::read(t.0.join("state/pairs/acme-helper.key")).unwrap();
    let (code, v) = run(&t.0, &["twin", "mirror", "--pair", "acme-helper", "--off"]);
    assert_eq!(code, 0, "{v}");
    assert_ne!(
        fs::read(t.0.join("state/pairs/acme-helper.key")).unwrap(),
        key0
    );
    run(&t.0, &["twin", "demo", "--step", "rename"]);
    let (_, v) = run(&t.0, &["twin", "sync", "--pair", "acme-helper"]);
    assert_ne!(pair(&v, "acme-helper")["state"], "error", "{v}");
    let (code, v) = run(
        &t.0,
        &[
            "twin",
            "mirror",
            "--pair",
            "acme-helper",
            "--pair-folder",
            drive.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{v}");
    run(&t.0, &["twin", "demo", "--step", "reset"]);
    let (_, v) = run(&t.0, &["twin", "sync", "--pair", "acme-helper"]);
    assert_ne!(pair(&v, "acme-helper")["state"], "error", "{v}");
}

#[test]
fn disconnecting_the_mirror_bot_lists_what_was_in_flight() {
    let t = Tmp::new("twin-off-in-flight");
    run(&t.0, &["twin", "demo"]);
    let drive = t.0.join("drive/acme-pair");
    fs::create_dir_all(&drive).unwrap();
    run(
        &t.0,
        &[
            "twin",
            "mirror",
            "--pair",
            "acme-helper",
            "--pair-folder",
            drive.to_str().unwrap(),
        ],
    );
    let sk =
        t.0.join("home/profiles/acme-helper/skills/grok-twin/greet-the-team");
    fs::create_dir_all(&sk).unwrap();
    fs::write(sk.join("SKILL.md"), "---\nname: greet-the-team\ndescription: \"Say hello to the whole team\"\n---\n\nHello, team.\n").unwrap();
    run(&t.0, &["twin", "sync", "--pair", "acme-helper"]);
    let (_, v) = run(
        &t.0,
        &[
            "twin",
            "approve",
            "--pair",
            "acme-helper",
            "--item",
            "skill:greet-the-team",
        ],
    );
    assert_eq!(data(&v)["sync"]["to_grok"], 1, "{v}");
    // the Mirror bot never picked it up; the owner disconnects it
    let (code, v) = run(&t.0, &["twin", "mirror", "--pair", "acme-helper", "--off"]);
    assert_eq!(code, 0, "{v}");
    let (_, v) = run(&t.0, &["twin", "sync", "--pair", "acme-helper"]);
    assert_ne!(pair(&v, "acme-helper")["state"], "error", "{v}");
    let (_, v) = run(&t.0, &["twin", "status", "--pair", "acme-helper"]);
    let s = pair(&v, "acme-helper");
    assert_eq!(s["state"], "changes_waiting", "{s}");
    assert!(
        s["by_hand"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["item"] == "skill:greet-the-team" && b["side"] == "grok"),
        "{s}"
    );
}

#[test]
fn keep_grok_then_restore_brings_the_hermes_copy_back_and_own_writes_are_no_news() {
    let t = Tmp::new("twin-keep-grok");
    run(&t.0, &["twin", "demo"]);
    // a sync's own writes do not show up as a change at the next check
    run(&t.0, &["twin", "demo", "--step", "rename"]);
    let (_, v) = run(&t.0, &["twin", "check"]);
    assert_eq!(data(&v)["changed"].as_array().unwrap().len(), 0, "{v}");
    run(&t.0, &["twin", "demo", "--step", "conflict"]);
    let skill =
        t.0.join("home/profiles/acme-helper/skills/grok-twin/greet-visitors/SKILL.md");
    assert!(fs::read_to_string(&skill)
        .unwrap()
        .contains("Always end with"));
    let (code, v) = run(
        &t.0,
        &[
            "twin",
            "resolve",
            "--pair",
            "acme-helper",
            "--conflict",
            "skill:greet-visitors",
            "--keep",
            "grok",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert!(!fs::read_to_string(&skill)
        .unwrap()
        .contains("Always end with"));
    // the Hermes copy it replaced is a restore point of its own
    let (_, v) = run(&t.0, &["twin", "restore-points", "--pair", "acme-helper"]);
    let points: Vec<String> = data(&v)["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["item"] == "skill:greet-visitors")
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    assert!(points.len() >= 2, "{v}");
    let mut restored = false;
    for p in &points {
        run(
            &t.0,
            &["twin", "restore", "--pair", "acme-helper", "--point", p],
        );
        if fs::read_to_string(&skill)
            .unwrap()
            .contains("Always end with")
        {
            restored = true;
            break;
        }
    }
    assert!(restored, "no restore point held the Hermes copy");
}

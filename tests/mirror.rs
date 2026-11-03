//! End-to-end tests of `botport mirror ...` on invented fixtures.
mod common;
use common::{s, w, Tmp};
use std::process::Command;

fn bin(state: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_botport"));
    c.env("XDG_STATE_HOME", state);
    c
}

#[test]
fn stats_summarises_the_metrics_file() {
    let t = Tmp::new("mirror-stats");
    let f = t.0.join("m.csv");
    let mut csv = String::from(botport::core::metrics::HEADER);
    csv.push('\n');
    for (i, ms) in [120, 80, 300].iter().enumerate() {
        csv.push_str(&format!(
            "2000-01-03T05:0{i}:00Z,demo-bot,mirror-sync,drive,drive,30,1,0,0,100,10,{ms},40,{},0,{}\n",
            ms - 40,
            20 + i
        ));
    }
    csv.push_str("not,a,row\n");
    w(&f, &csv);
    let o = bin(&t.0)
        .args(["mirror", "stats", "--metrics", s(&f), "--json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let g = &v["groups"][0];
    assert_eq!(
        (
            g["runs"].as_u64(),
            g["p50_ms"].as_u64(),
            g["p95_ms"].as_u64()
        ),
        (Some(3), Some(120), Some(300))
    );
    assert_eq!(g["local_tokens"], 0);
    // an old window leaves nothing
    let o = bin(&t.0)
        .args(["mirror", "stats", "--metrics", s(&f), "--since", "1m"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("no mirror runs"));
    let o = bin(&t.0)
        .args(["mirror", "stats", "--since", "7w"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    let o = bin(&t.0).args(["mirror", "nope"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
}

// ---------------------------------------------------------------- the whole loop on invented fixtures

use std::path::{Path, PathBuf};

/// A stand-in `hermes` that keeps cron/jobs.json of the profile in $FAKE_PROFILE_DIR (create/edit/pause/resume/remove).
const FAKE_HERMES: &str = r#"#!/usr/bin/env python3
import json, os, sys
a = sys.argv[1:]
if a[:1] == ["--version"]:
    print("Hermes Agent v0.0.0 (test)"); sys.exit(0)
sub, rest = a[3], a[4:]
p = os.path.join(os.environ["FAKE_PROFILE_DIR"], "cron", "jobs.json")
os.makedirs(os.path.dirname(p), exist_ok=True)
jobs = json.load(open(p))["jobs"] if os.path.exists(p) else []
def opt(n):
    for i, x in enumerate(rest):
        if x.startswith(n + "="):
            del rest[i]; return x[len(n) + 1:]
        if x == n:
            v = rest[i + 1]; del rest[i:i + 2]; return v
def flag(n):
    if n in rest:
        rest.remove(n); return True
    return False
def positionals():
    while "--" in rest:
        rest.remove("--")
def find(j):
    return next(x for x in jobs if x["id"] == j or x["name"] == j)
if sub == "create":
    name = opt("--name"); opt("--deliver"); tz = opt("--timezone"); paused = flag("--paused"); reason = opt("--paused-reason")
    positionals()
    jobs.append({"id": "j%d" % (len(jobs) + 1), "name": name, "prompt": rest[1], "schedule": {"expr": rest[0]},
                 "enabled": not paused, "timezone": tz, "paused_reason": reason})
elif sub == "edit":
    s = opt("--schedule"); pr = opt("--prompt"); tz = opt("--timezone"); positionals(); j = find(rest.pop(0))
    if s: j["schedule"]["expr"] = s
    if pr is not None: j["prompt"] = pr
    if tz: j["timezone"] = tz
elif sub in ("pause", "resume"):
    positionals(); j = find(rest[0]); j["enabled"] = sub == "resume"
    if sub == "resume": j["paused_reason"] = None
elif sub == "remove":
    positionals(); j = find(rest[0]); jobs.remove(j)
json.dump({"jobs": jobs}, open(p, "w"))
"#;

struct Pair {
    t: Tmp,
    export: PathBuf,
    hermes: PathBuf,
    drive: PathBuf,
    cfg: PathBuf,
    fake: PathBuf,
}

fn export_with(instructions: &str, routines: &[(&str, &str, bool)]) -> String {
    let routines: Vec<serde_json::Value> = routines
        .iter()
        .map(|(name, prompt, paused)| {
            serde_json::json!({"name": name, "instruction": prompt, "trigger": "schedule",
            "schedule_cron": "0 9 * * 1", "timezone": "UTC", "paused": paused})
        })
        .collect();
    serde_json::json!({"format": "grok-bot-export", "version": 1, "bots": [{
        "name": "Demo Bot", "instructions": instructions, "skills": [], "routines": routines, "apps": []}]})
    .to_string()
}

fn export_json(instructions: &str, prompt: &str, skills: usize, routines: usize) -> String {
    let skills: Vec<serde_json::Value> = (0..skills)
        .map(|i| {
            serde_json::json!({"name": format!("demo-skill-{i}"), "description": "Example skill",
                                    "instructions": format!("# Demo skill {i}\nExample steps.")})
        })
        .collect();
    let mut routines: Vec<serde_json::Value> = (0..routines)
        .map(|i| serde_json::json!({"name": format!("demo-routine-{i}"), "instruction": format!("Example prompt {i}."),
                                    "trigger": "schedule", "schedule_cron": format!("{} 9 * * 1", i % 60), "timezone": "UTC", "paused": false}))
        .collect();
    if routines.is_empty() {
        routines.push(serde_json::json!({"name": "demo-routine", "instruction": prompt, "trigger": "schedule",
                                         "schedule_cron": "0 9 * * 1", "timezone": "UTC", "paused": false}));
    }
    serde_json::json!({"format": "grok-bot-export", "version": 1, "bots": [{
        "name": "Demo Bot", "instructions": instructions, "skills": skills, "routines": routines, "apps": []}]})
    .to_string()
}

impl Pair {
    fn new(tag: &str) -> Pair {
        let t = Tmp::new(tag);
        let p = Pair {
            export: t.0.join("box/export.json"),
            hermes: t.0.join("hermes"),
            drive: t.0.join("drive"),
            cfg: t.0.join("cfg/demo-bot.toml"),
            fake: t.0.join("fake-hermes"),
            t,
        };
        std::fs::create_dir_all(&p.hermes).unwrap();
        std::fs::create_dir_all(&p.drive).unwrap();
        w(&p.fake, FAKE_HERMES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p.fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn bp(&self, args: &[&str]) -> (i32, serde_json::Value, String) {
        let mut all: Vec<&str> = args.to_vec();
        if args[0] == "mirror" && !matches!(args[1], "digest-items" | "apply-pack") {
            all.extend(["--pair", "demo-bot", "--mirror-config", s(&self.cfg)]);
        }
        // these flows test carrying without per-change approval, which the owner opts into (the default holds every
        // change for approval: `approval_is_the_default`)
        if args[..2] == ["mirror", "init"]
            && !args.contains(&"--approve")
            && !args.contains(&"--undo")
            && !args.contains(&"--default-approval")
        {
            all.extend(["--approve", "runnable"]);
        }
        all.retain(|a| *a != "--default-approval");
        // the Grok box signs with the pair's key, when init made one
        let key = self.cfg.with_extension("key");
        if args[0] == "mirror"
            && matches!(args[1], "digest-items" | "apply-pack")
            && key.exists()
            && !args.contains(&"--key")
        {
            all.extend(["--key", s(&key)]);
        }
        all.push("--json");
        let o = bin(&self.t.0)
            .args(&all)
            .env("FAKE_PROFILE_DIR", &self.hermes)
            .env("XDG_CONFIG_HOME", self.t.0.join("xdg-config"))
            .output()
            .unwrap();
        let v = serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null);
        (
            o.status.code().unwrap_or(-1),
            v,
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }

    /// The Grok routine's part: state file (and changed blobs) into the Drive folder, with the applied log.
    fn box_digest(&self) {
        self.box_digest_with("complete");
    }

    /// The box's state file, with `coverage`: `complete` stands for a program read of the live setup, `partial` (the
    /// default for an export) for the bot's own export.
    fn box_digest_with(&self, coverage: &str) {
        let log = self.t.0.join("box/applied.jsonl");
        let (c, _, e) = self.bp(&[
            "mirror",
            "digest-items",
            "--pair",
            "demo-bot",
            "--from",
            s(&self.export),
            "--out",
            s(&self.drive.join("from-grok")),
            "--log",
            s(&log),
            "--coverage",
            coverage,
        ]);
        assert_eq!(c, 0, "{e}");
    }

    fn box_apply(&self) -> serde_json::Value {
        let (c, v, e) = self.bp(&[
            "mirror",
            "apply-pack",
            "--pair",
            "demo-bot",
            "--dir",
            s(&self.drive),
            "--from",
            s(&self.export),
            "--stage",
            s(&self.t.0.join("box/stage")),
            "--log",
            s(&self.t.0.join("box/applied.jsonl")),
        ]);
        assert_eq!(c, 0, "{e}");
        v
    }

    fn init(&self) -> serde_json::Value {
        let (c, v, e) = self.init_signed(&[]);
        assert_eq!(c, 0, "{e}");
        v
    }

    /// `mirror init` as the docs describe it for a signed pair: the first run makes the pair key, the Grok box
    /// writes its state file again with it, and the second run takes over the profile.
    fn init_signed(&self, extra: &[&str]) -> (i32, serde_json::Value, String) {
        let r = init_with(self, extra);
        if r.0 == 2 && r.2.contains("a pair key was made") {
            self.box_digest();
            return init_with(self, extra);
        }
        r
    }
    fn sync(&self) -> (i32, serde_json::Value, String) {
        self.bp(&["mirror", "sync", "--metrics", s(&self.t.0.join("m.csv"))])
    }
}

fn soul(p: &Path) -> String {
    std::fs::read_to_string(p.join("SOUL.md")).unwrap_or_default()
}

#[test]
fn equal_peer_loop_end_to_end() {
    let p = Pair::new("mirror-loop");
    // instructions of several lines: a one-line edit stays under the approval threshold
    let base: String = (1..=8)
        .map(|i| format!("Example instruction line {i}.\n"))
        .collect();
    w(
        &p.export,
        &export_json(base.trim_end(), "Example prompt.", 1, 0),
    );
    p.box_digest();
    // first sync is never a blind copy: every one-sided item goes to the pick-one list
    let v = p.init();
    assert_eq!(
        (v["in_step"].as_u64(), v["to_pick"].as_u64()),
        (Some(0), Some(4)),
        "{v}"
    );
    let (c, _, _) = p.sync();
    assert_eq!(c, 3, "open conflicts");
    for it in [
        "persona.name",
        "persona.instructions",
        "skill:demo-skill-0",
        "routine:demo-routine",
    ] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    assert!(
        soul(&p.hermes).contains("Example instruction line 8."),
        "{}",
        soul(&p.hermes)
    );
    assert!(p
        .hermes
        .join("skills/mirror/demo-skill-0/SKILL.md")
        .is_file());
    let jobs = std::fs::read_to_string(p.hermes.join("cron/jobs.json")).unwrap();
    assert!(jobs.contains("demo-routine"));
    // the picked routine arrived enabled, so it is held paused until approved
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["changed"], 0);
    let (c, _, _) = p.bp(&["mirror", "status"]);
    assert_eq!(c, 0);

    // a Hermes-side edit to the instructions goes out as a change pack
    let s0 = soul(&p.hermes);
    w(
        &p.hermes.join("SOUL.md"),
        &s0.replace(
            "Example instruction line 1.",
            "Example instruction line 1, edited on Hermes.",
        ),
    );
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["gen"], 1);
    assert!(p.drive.join("HEAD").is_file());
    // the Grok routine stages it; the bot installs it (here: edits its export); the next digest logs it as applied
    let v = p.box_apply();
    assert_eq!(v["pending"], 1, "{v}");
    let staged = std::fs::read_to_string(p.t.0.join("box/stage/persona.instructions.txt")).unwrap();
    assert!(staged.contains("edited on Hermes"));
    w(
        &p.export,
        &export_json(staged.trim_end(), "Example prompt.", 1, 0),
    );
    let v = p.box_apply();
    assert_eq!(v["pending"], 0, "{v}");
    p.box_digest();
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["in_flight"], 0, "{v}");

    // a Grok-side prompt change to a routine enabled on Hermes waits for approval, then flows
    w(
        &p.export,
        &export_json(staged.trim_end(), "Example prompt, changed on Grok.", 1, 0),
    );
    p.box_digest();
    let (c, v, _) = p.sync();
    assert_eq!((c, v["waiting_for_approval"].as_u64()), (3, Some(1)), "{v}");
    let report = std::fs::read_to_string(p.hermes.join("botport-mirror-conflicts.md")).unwrap();
    assert!(report.contains("routine:demo-routine needs approval"));
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert!(std::fs::read_to_string(p.hermes.join("cron/jobs.json"))
        .unwrap()
        .contains("changed on Grok"));

    // both sides change the instructions: a conflict, nothing overwritten; keep hermes publishes a pack
    w(
        &p.export,
        &export_json(
            &staged
                .trim_end()
                .replace("line 2.", "line 2, Grok wording."),
            "Example prompt, changed on Grok.",
            1,
            0,
        ),
    );
    p.box_digest();
    let s1 = soul(&p.hermes);
    let hermes_text = s1.replace("line 2.", "line 2, Hermes wording.");
    w(&p.hermes.join("SOUL.md"), &hermes_text);
    let (c, v, _) = p.sync();
    assert_eq!((c, v["conflicts"].as_u64()), (3, Some(1)), "{v}");
    assert_eq!(soul(&p.hermes), hermes_text, "a conflict never overwrites");
    let (c, _, e) = p.bp(&[
        "mirror",
        "pick",
        "persona.instructions",
        "--keep",
        "hermes",
        "--yes",
    ]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(
        v["in_flight"], 1,
        "the pick is on its way to the Grok side: {v}"
    );
    let v = p.box_apply();
    assert_eq!(v["pending"], 1);

    // the metrics file has one row per sync, no local model tokens
    let rows = botport::core::metrics::read(&p.t.0.join("m.csv")).unwrap();
    assert!(rows.len() >= 6);
    assert!(rows
        .iter()
        .all(|r| r.local_tokens == 0 && r.cmd == "mirror-sync"));
    // the state file holds digests and names only
    let st = std::fs::read_to_string(p.hermes.join("botport.mirror.json")).unwrap();
    assert!(!st.contains("wording") && !st.contains("Example prompt"));
}

#[test]
fn v01_commands_refuse_a_mirrored_profile_until_undo() {
    let p = Pair::new("mirror-undo");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    let o = bin(&p.t.0)
        .args([
            "plan",
            "--bundle",
            s(&p.export),
            "--profile-dir",
            s(&p.hermes),
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("mirrored profile"));
    let (c, _, e) = p.bp(&["mirror", "init", "--undo"]);
    assert_eq!(c, 0, "{e}");
    assert!(!p.hermes.join("botport.mirror.json").exists());
    let o = bin(&p.t.0)
        .args([
            "plan",
            "--bundle",
            s(&p.export),
            "--profile-dir",
            s(&p.hermes),
        ])
        .output()
        .unwrap();
    assert_ne!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn job<'a>(jobs: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["name"] == name)
        .unwrap()
}

fn jobs(p: &Pair) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(p.hermes.join("cron/jobs.json")).unwrap())
        .unwrap()
}

#[test]
fn routines_pause_and_enable_across_sides() {
    let p = Pair::new("mirror-routines-onoff");
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", false)],
        ),
    );
    p.box_digest();
    p.init();
    for it in [
        "persona.name",
        "persona.instructions",
        "routine:demo-routine",
    ] {
        assert_eq!(
            p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]).0,
            0,
            "{it}"
        );
    }
    // picked while enabled on the Grok side: written held paused, in step, listed until approved
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], false);
    let (c, v, e) = p.sync();
    assert_eq!(
        (c, v["changed"].as_u64(), v["waiting_for_approval"].as_u64()),
        (3, Some(0), Some(1)),
        "{v} {e}"
    );
    let report = std::fs::read_to_string(p.hermes.join("botport-mirror-conflicts.md")).unwrap();
    assert!(
        report.contains("routine:demo-routine is paused on the Hermes side until you approve it")
    );
    // the owner pauses it on the Grok side while it is still held: carried as a pause, nothing switches on
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", true)],
        ),
    );
    p.box_digest();
    let (c, v, e) = p.sync();
    assert_eq!(
        (c, v["waiting_for_approval"].as_u64()),
        (0, Some(0)),
        "{v} {e}"
    );
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], false);
    // switched back on there: held again until approved here
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", false)],
        ),
    );
    p.box_digest();
    let (c, v, _) = p.sync();
    assert_eq!((c, v["waiting_for_approval"].as_u64()), (3, Some(1)), "{v}");
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], false);
    assert_eq!(
        p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"])
            .0,
        0
    );
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], true);
    assert_eq!(p.sync().0, 0);
    // the owner pauses it on the Grok side: carried, no approval needed
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", true)],
        ),
    );
    p.box_digest();
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], false);
    // switching it back on there needs the owner's approval here
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", false)],
        ),
    );
    p.box_digest();
    let (c, v, _) = p.sync();
    assert_eq!((c, v["waiting_for_approval"].as_u64()), (3, Some(1)), "{v}");
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], false);
    assert_eq!(
        p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"])
            .0,
        0
    );
    assert_eq!(p.sync().0, 0);
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], true);
    // a new routine whose prompt looks like a command-line option is written as text (held paused), nothing else
    let odd = "--deliver=example-channel run the demo";
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[
                ("demo-routine", "Example prompt.", false),
                ("odd-routine", odd, false),
            ],
        ),
    );
    p.box_digest();
    let (c, v, e) = p.sync();
    assert_eq!(
        (c, v["waiting_for_approval"].as_u64()),
        (3, Some(1)),
        "{v} {e}"
    );
    let j = jobs(&p);
    let o = job(&j, "odd-routine");
    assert_eq!(
        (o["prompt"].as_str(), o["enabled"].as_bool()),
        (Some(odd), Some(false)),
        "{o}"
    );
}

#[test]
fn a_hold_ends_when_the_owner_changes_the_job_by_hand() {
    let p = Pair::new("mirror-hold-ends");
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", false)],
        ),
    );
    p.box_digest();
    p.init();
    for it in [
        "persona.name",
        "persona.instructions",
        "routine:demo-routine",
    ] {
        assert_eq!(
            p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]).0,
            0,
            "{it}"
        );
    }
    assert_eq!(p.sync().1["waiting_for_approval"].as_u64(), Some(1));
    // the owner switches it on by hand: the hold is over, the job is in step with the enabled Grok copy
    let set = |enabled: bool| {
        let mut j = jobs(&p);
        for x in j["jobs"].as_array_mut().unwrap() {
            x["enabled"] = enabled.into();
            if enabled {
                x["paused_reason"] = serde_json::Value::Null;
            }
        }
        w(&p.hermes.join("cron/jobs.json"), &j.to_string());
    };
    set(true);
    let (c, v, e) = p.sync();
    assert_eq!(
        (
            c,
            v["waiting_for_approval"].as_u64(),
            v["in_flight"].as_u64()
        ),
        (0, Some(0), Some(0)),
        "{v} {e}"
    );
    assert!(!report(&p).contains("paused on the Hermes side"));
    // restoring the kept (enabled) copy of a running routine keeps it running, not held
    let store = p.hermes.join("botport/mirror-store").join(
        "routine:demo-routine"
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    );
    let short: String = std::fs::read_dir(&store)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .file_name()
        .to_string_lossy()[..12]
        .into();
    let (c, _, e) = p.bp(&[
        "mirror",
        "restore",
        "routine:demo-routine",
        "--from",
        &short,
        "--yes",
    ]);
    assert_eq!(c, 0, "{e}");
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], true);
    let (c, v, e) = p.sync();
    assert_eq!(
        (c, v["waiting_for_approval"].as_u64()),
        (0, Some(0)),
        "{v} {e}"
    );
    // then pauses it by hand: a pause is carried to the Grok side, not mistaken for a held copy
    set(false);
    let (c, v, e) = p.sync();
    assert_eq!(
        (
            c,
            v["waiting_for_approval"].as_u64(),
            v["in_flight"].as_u64()
        ),
        (0, Some(0), Some(1)),
        "{v} {e}"
    );
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_ne!(c, 0, "nothing to switch on: {e}");
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], false);
}

#[test]
fn a_grok_side_read_from_its_export_is_add_only() {
    let p = Pair::new("mirror-add-only");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    for it in [
        "persona.name",
        "persona.instructions",
        "skill:demo-skill-0",
        "routine:demo-routine",
    ] {
        assert_eq!(
            p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]).0,
            0,
            "{it}"
        );
    }
    // from now on the box reports from its export: partial coverage
    p.box_digest_with("partial");
    p.sync();
    // the owner edits a skill the Grok side has, and adds a new one, on the Hermes side
    w(
        &p.hermes.join("skills/mirror/demo-skill-0/SKILL.md"),
        "# Demo skill 0\nChanged steps.\n",
    );
    w(
        &p.hermes.join("skills/mirror/demo-skill-new/SKILL.md"),
        "# New demo skill\nExample steps.\n",
    );
    let (_, v, e) = p.sync();
    let item = |v: &serde_json::Value, id: &str| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["item"] == id)
            .cloned()
            .unwrap_or_default()
    };
    // a replacement there is the owner's to make by hand; the reason is in the output and the report
    let edited = item(&v, "skill:demo-skill-0");
    assert_eq!(edited["action"], "unsupported", "{v} {e}");
    assert!(
        edited["why"].as_str().unwrap().contains("add-only"),
        "{edited}"
    );
    assert!(report(&p).contains("skill:demo-skill-0 is not carried: add-only"));
    // a new item can still go over, once the owner has checked the Grok bot has none by that name
    let new = item(&v, "skill:demo-skill-new");
    assert_eq!(new["action"], "needs-approval", "{v}");
    assert!(
        new["why"].as_str().unwrap().contains("may have left out"),
        "{new}"
    );
    let (c, _, e) = p.bp(&[
        "mirror",
        "pick",
        "skill:demo-skill-0",
        "--keep",
        "hermes",
        "--yes",
    ]);
    assert_ne!(c, 0, "{e}");
    // the export leaves the skill out: unseen, not removed on the Hermes side
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 0, 0),
    );
    p.box_digest_with("partial");
    let (_, v, _) = p.sync();
    assert!(p.hermes.join("skills/mirror/demo-skill-0").is_dir());
    assert_ne!(
        item(&v, "skill:demo-skill-0")["action"],
        "retire-local",
        "{v}"
    );
}

#[test]
fn an_export_that_leaves_the_persona_out_never_gets_it_replaced() {
    let p = Pair::new("mirror-persona-omitted");
    // the export lacks the instructions (left out), the Hermes side has its own
    w(&p.export, &export_with("", &[]));
    w(
        &p.hermes.join("SOUL.md"),
        "<!-- botport:begin persona -->\nExample Hermes instructions.\n<!-- botport:end persona -->\n",
    );
    p.box_digest_with("partial");
    let (c, _, e) = p.init_signed(&["--copy-one-sided"]);
    assert_eq!(c, 0, "{e}");
    p.box_digest_with("partial");
    let (_, v, e) = p.sync();
    let ins = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["item"] == "persona.instructions")
        .cloned()
        .unwrap_or_default();
    assert_eq!(ins["action"], "unsupported", "{v} {e}");
    assert!(
        ins["why"]
            .as_str()
            .unwrap()
            .contains("always has its persona"),
        "{ins}"
    );
    assert_eq!(v["gen"], serde_json::Value::Null, "nothing published: {v}");
    // nor can a pick send it: on the pick-one list (init without --copy-one-sided), keeping the Hermes copy is refused
    let (c, _, e) = init_with(&p, &["--force"]);
    assert_eq!(c, 0, "{e}");
    assert!(
        report(&p).contains("(refused while the Grok side is read from its export"),
        "{}",
        report(&p)
    );
    let (c, _, e) = p.bp(&[
        "mirror",
        "pick",
        "persona.instructions",
        "--keep",
        "hermes",
        "--yes",
    ]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("always has its persona"), "{e}");
    // and the unsigned/signed choice is one or the other
    let (c, _, e) = init_with(
        &p,
        &["--force", "--no-pair-key", "--key-file", "/nonexistent"],
    );
    assert_eq!(c, 2);
    assert!(e.contains("one of --key-file F or --no-pair-key"), "{e}");
}

#[test]
fn no_state_no_merge_and_wrong_pair() {
    let p = Pair::new("mirror-nostate");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    std::fs::remove_file(p.hermes.join("botport.mirror.json")).unwrap();
    let (c, _, e) = p.sync();
    assert_eq!(c, 2);
    assert!(e.contains("mirror init"), "{e}");
}

#[test]
fn a_quiet_sync_of_a_large_bot_is_fast() {
    let p = Pair::new("mirror-perf");
    w(&p.export, &export_json("Example instructions.", "", 50, 20));
    p.box_digest();
    // the Hermes side starts as an exact copy, so init records every item as in step
    let items = botport::adapters::grok_bot::mirror::read_items(&p.export).unwrap();
    let mut jobs = vec![];
    for (id, c) in &items.content {
        match c {
            botport::core::item::Content::Package(pk) => {
                pk.write(&p.hermes.join("skills/mirror"), false).unwrap()
            }
            botport::core::item::Content::Routine(r) => jobs.push(serde_json::json!({
                "id": format!("j{}", jobs.len()), "name": r.name, "prompt": r.action,
                "schedule": {"expr": r.cron}, "enabled": r.enabled, "timezone": r.tz})),
            botport::core::item::Content::Text(t) => {
                let sec = if id.as_str() == "persona.name" {
                    "persona-name"
                } else {
                    "persona"
                };
                let cur = soul(&p.hermes);
                w(
                    &p.hermes.join("SOUL.md"),
                    &format!(
                        "{cur}<!-- botport:begin {sec} -->\n{t}\n<!-- botport:end {sec} -->\n"
                    ),
                );
            }
            _ => {}
        }
    }
    w(
        &p.hermes.join("cron/jobs.json"),
        &serde_json::json!({"jobs": jobs}).to_string(),
    );
    let v = p.init();
    assert_eq!(v["to_pick"], 0, "{v}");
    assert_eq!(v["in_step"].as_u64(), Some(72));
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{e}");
    let ms = v["wall_ms"].as_u64().unwrap();
    assert!(
        ms < 1000,
        "a quiet sync of 50 skills and 20 routines took {ms} ms"
    );
}

#[test]
fn deletions_from_an_unsigned_read_wait_for_approval() {
    let p = Pair::new("mirror-retire");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    p.box_digest();
    // an unsigned pair, as the owner may still choose
    let (c, _, e) = p.init_signed(&["--no-pair-key"]);
    assert_eq!(c, 0, "{e}");
    assert!(!p.cfg.with_extension("key").exists());
    for it in [
        "persona.name",
        "persona.instructions",
        "routine:demo-routine",
    ] {
        assert_eq!(
            p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]).0,
            0,
            "{it}"
        );
    }
    // skills (packages) never travel in an unsigned pair
    let (c, _, e) = p.bp(&[
        "mirror",
        "pick",
        "skill:demo-skill-0",
        "--keep",
        "grok",
        "--yes",
    ]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("signed pair"), "{e}");
    assert_eq!(
        p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"])
            .0,
        0
    );
    let reason = |v: &serde_json::Value, id: &str| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["item"] == id)
            .and_then(|i| i["why"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    // the routine changes on the Grok side: not signed, so it does not overwrite the Hermes copy on its own
    w(
        &p.export,
        &export_json("Example instructions.", "Changed prompt.", 1, 0),
    );
    p.box_digest();
    let (c, v, _) = p.sync();
    assert_eq!(c, 3, "{v}");
    assert!(
        reason(&v, "routine:demo-routine")
            .contains("changed on the Grok side, in a read not signed"),
        "{v}"
    );
    assert_eq!(job(&jobs(&p), "demo-routine")["prompt"], "Example prompt.");
    // it disappears there: not retired on its own either
    w(&p.export, &export_with("Example instructions.", &[]));
    p.box_digest();
    let (c, v, _) = p.sync();
    assert_eq!(c, 3, "{v}");
    let report = std::fs::read_to_string(p.hermes.join("botport-mirror-conflicts.md")).unwrap();
    assert!(report.contains("not signed with the pair key"), "{report}");
    assert!(jobs(&p)["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|j| j["name"] == "demo-routine"));
    assert_eq!(
        p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"])
            .0,
        0
    );
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert!(!jobs(&p)["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|j| j["name"] == "demo-routine"));
}

#[test]
fn a_signed_pair_ignores_unsigned_files_and_replayed_triggers() {
    let p = Pair::new("mirror-signed");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    // init with a new pair key: the unsigned state file already there does not count
    p.box_digest();
    let (c, _, e) = p.bp(&[
        "mirror",
        "init",
        "--profile-dir",
        s(&p.hermes),
        "--hermes-bin",
        s(&p.fake),
        "--transport",
        "drive",
        "--drive-dir",
        s(&p.drive),
    ]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("nothing from the Grok side"), "{e}");
    let key_file = p.cfg.with_extension("key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // the Grok box signs with its copy of the key
    let (c, _, e) = p.bp(&[
        "mirror",
        "digest-items",
        "--pair",
        "demo-bot",
        "--from",
        s(&p.export),
        "--out",
        s(&p.drive.join("from-grok")),
        "--key",
        s(&key_file),
        "--coverage",
        "complete",
    ]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.bp(&[
        "mirror",
        "init",
        "--profile-dir",
        s(&p.hermes),
        "--hermes-bin",
        s(&p.fake),
        "--transport",
        "drive",
        "--drive-dir",
        s(&p.drive),
    ]);
    assert_eq!(c, 0, "{e}");
    assert_eq!(v["to_pick"], 4);
    // a host-triggered sync needs a fresh trigger signed with the key, used once
    let key = botport::core::auth::parse_key(&std::fs::read_to_string(&key_file).unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let tr = botport::core::auth::Trigger::new(&key, "demo-bot", "nonce-a", now);
    let tf = p.t.0.join("trigger.json");
    w(&tf, &serde_json::to_string(&tr).unwrap());
    let run = |f: &std::path::Path| {
        p.bp(&[
            "mirror",
            "sync",
            "--trigger",
            s(f),
            "--metrics",
            s(&p.t.0.join("m.csv")),
        ])
    };
    let (c, _, e) = run(&tf);
    assert_eq!(c, 3, "{e}"); // runs; the four one-sided items are still to pick
    let (c, _, e) = run(&tf);
    assert_eq!(c, 2);
    assert!(e.contains("already used"), "{e}");
    let forged = botport::core::auth::Trigger::new(&[0u8; 32], "demo-bot", "nonce-b", now);
    w(&tf, &serde_json::to_string(&forged).unwrap());
    assert_eq!(run(&tf).0, 2);
    // keeping the Hermes side's absence publishes a signed retire; apply-pack in the box checks the signature
    assert_eq!(
        p.bp(&[
            "mirror",
            "pick",
            "persona.instructions",
            "--keep",
            "hermes",
            "--yes"
        ])
        .0,
        0
    );
    let (c, v, e) = p.bp(&["mirror", "sync", "--metrics", s(&p.t.0.join("m.csv"))]);
    assert_eq!(c, 3, "{v} {e}");
    assert_eq!(v["gen"], 1, "{v}");
    let apply = |k: &std::path::Path| {
        p.bp(&[
            "mirror",
            "apply-pack",
            "--pair",
            "demo-bot",
            "--dir",
            s(&p.drive),
            "--from",
            s(&p.export),
            "--stage",
            s(&p.t.0.join("box/stage")),
            "--key",
            s(k),
        ])
    };
    let (c, v, e) = apply(&key_file);
    assert_eq!(c, 0, "{e}");
    assert_eq!(v["pending"], 1);
    let other = p.t.0.join("other.key");
    w(&other, &"ab".repeat(32));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (c, _, e) = apply(&other);
    assert_eq!(c, 2);
    assert!(e.contains("signature"), "{e}");
}

fn report(p: &Pair) -> String {
    std::fs::read_to_string(p.hermes.join("botport-mirror-conflicts.md")).unwrap()
}

fn init_with(p: &Pair, extra: &[&str]) -> (i32, serde_json::Value, String) {
    let mut args = vec![
        "mirror",
        "init",
        "--profile-dir",
        s(&p.hermes),
        "--hermes-bin",
        s(&p.fake),
        "--transport",
        "drive",
        "--drive-dir",
        s(&p.drive),
    ];
    args.extend(extra);
    let metrics = p.t.0.join("m.csv");
    args.extend(["--metrics", s(&metrics)]);
    p.bp(&args)
}

#[test]
fn init_lists_one_sided_items_with_commands_that_work_as_printed() {
    let p = Pair::new("mirror-one-sided");
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("Example routine", "Example prompt.", false)],
        ),
    );
    p.box_digest();
    let v = p.init();
    assert_eq!(v["to_pick"], 3, "{v}");
    let r = report(&p);
    assert!(
        r.contains("## routine:Example routine  (only on the grok side, grok "),
        "{r}"
    );
    assert!(r.contains(
        "Copy it to hermes: botport mirror pick --pair demo-bot 'routine:Example routine' --keep grok\n"
    ));
    assert!(r.contains(
        "Remove it from grok: botport mirror pick --pair demo-bot 'routine:Example routine' --keep hermes\n"
    ));
    assert!(r.contains("Compare: botport mirror show --pair demo-bot persona.instructions\n"));
    assert!(!r.contains("both sides changed"), "{r}");
    // the quoted id is one argument: the printed command finds the item
    let (c, _, e) = p.bp(&["mirror", "show", "routine:Example routine"]);
    assert_eq!(c, 0, "{e}");
}

#[test]
fn init_can_leave_one_sided_items_to_the_first_sync() {
    let p = Pair::new("mirror-copy-one-sided");
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", false)],
        ),
    );
    p.box_digest();
    let (c, v, e) = p.init_signed(&["--copy-one-sided"]);
    assert_eq!((c, v["to_pick"].as_u64()), (0, Some(0)), "{v} {e}");
    // copied, under the approval rules: the name is written, the instructions wait, the routine arrives held paused
    let (c, v, e) = p.sync();
    assert_eq!(
        (
            c,
            v["conflicts"].as_u64(),
            v["changed"].as_u64(),
            v["waiting_for_approval"].as_u64()
        ),
        (3, Some(0), Some(2), Some(2)),
        "{v} {e}"
    );
    assert_eq!(job(&jobs(&p), "demo-routine")["enabled"], false);
    // the Hermes side now has two jobs with that name: it cannot offer either, which is not a deletion
    let mut j = jobs(&p);
    let mut twin = job(&j, "demo-routine").clone();
    twin["id"] = "j99".into();
    j["jobs"].as_array_mut().unwrap().push(twin);
    w(&p.hermes.join("cron/jobs.json"), &j.to_string());
    let (_, v, e) = p.sync();
    assert_eq!(
        v["in_flight"].as_u64(),
        Some(0),
        "nothing retired on the Grok side: {v} {e}"
    );
    let (_, plan, _) = p.bp(&["mirror", "plan"]);
    let r = plan["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["item"] == "routine:demo-routine")
        .cloned()
        .unwrap();
    assert_eq!(r["action"], "unsupported", "{plan}");
}

#[test]
fn init_says_where_it_looked() {
    let p = Pair::new("mirror-init-where");
    w(&p.export, &export_with("Example instructions.", &[]));
    // the Grok side wrote into the pair's folder; init is pointed at the folder above it
    let pair_dir = p.drive.join("demo-bot");
    let (c, _, e) = p.bp(&[
        "mirror",
        "digest-items",
        "--pair",
        "demo-bot",
        "--from",
        s(&p.export),
        "--out",
        s(&pair_dir.join("from-grok")),
    ]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = init_with(&p, &[]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("no from-grok/state-*.json under"), "{e}");
    assert!(e.contains("the pair's own folder"), "{e}");
    // state files written for another pair are named as such, not as a signing problem
    let (c, _, e) = p.bp(&[
        "mirror",
        "digest-items",
        "--pair",
        "other-pair",
        "--from",
        s(&p.export),
        "--out",
        s(&p.drive.join("from-grok")),
    ]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = init_with(&p, &[]);
    assert_eq!(c, 2);
    assert!(e.contains("belong to pair other-pair, not demo-bot"), "{e}");
    std::fs::create_dir_all(p.t.0.join("empty/demo-bot")).unwrap();
    // folders owned by the running user, writable by nobody else
    #[cfg(unix)]
    for d in ["empty", "empty/demo-bot"] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p.t.0.join(d), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let (c, _, e) = p.bp(&[
        "mirror",
        "init",
        "--profile-dir",
        s(&p.hermes),
        "--transport",
        "local",
        "--local-dir",
        s(&p.t.0.join("empty")),
    ]);
    assert_ne!(c, 0);
    assert!(e.contains("--transport hybrid"), "{e}");
}

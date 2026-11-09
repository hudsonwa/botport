//! End-to-end tests of `botport mirror ...` on invented fixtures.
mod common;
use common::{s, w, Tmp};
use std::process::Command;

fn bin(state: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_botport"));
    c.env("XDG_STATE_HOME", state);
    // never this machine's own Hermes gateway records
    c.env("HERMES_GATEWAY_LOCK_DIR", state.join("gateway-locks"));
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
fn watch_syncs_when_the_grok_side_writes_a_new_state() {
    let p = Pair::new("mirror-watch");
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Example prompt.", true)],
        ),
    );
    p.box_digest();
    let (c, _, e) = p.init_signed(&["--copy-one-sided"]);
    assert_eq!(c, 0, "{e}");
    p.sync();
    assert_eq!(job(&jobs(&p), "demo-routine")["prompt"], "Example prompt.");
    // the watcher runs for a few seconds; meanwhile the Grok side changes the (paused) routine's prompt
    let child = bin(&p.t.0)
        .args([
            "mirror",
            "watch",
            "--pair",
            "demo-bot",
            "--mirror-config",
            s(&p.cfg),
            "--interval",
            "100ms",
            "--for",
            "4s",
            "--json",
            "--metrics",
            s(&p.t.0.join("m.csv")),
        ])
        .env("FAKE_PROFILE_DIR", &p.hermes)
        .env("XDG_CONFIG_HOME", p.t.0.join("xdg-config"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("demo-routine", "Changed prompt.", true)],
        ),
    );
    p.box_digest();
    let o = child.wait_with_output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
    assert!(
        v["syncs"].as_u64().unwrap_or(0) >= 1,
        "{v} {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(job(&jobs(&p), "demo-routine")["prompt"], "Changed prompt.");
}

/// A changed pair configuration that cannot be read is retried on every poll (its change is not taken as seen): once
/// it can be read, watch reloads it and syncs. Meanwhile it says, once, that it runs on the previous configuration.
#[cfg(unix)]
#[test]
fn watch_retries_a_configuration_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;
    let p = Pair::new("mirror-watch-reload");
    w(&p.export, &export_with("Example instructions.", &[]));
    p.box_digest();
    let (c, _, e) = p.init_signed(&[]);
    assert_eq!(c, 0, "{e}");
    p.sync();
    let child = bin(&p.t.0)
        .args([
            "mirror",
            "watch",
            "--pair",
            "demo-bot",
            "--mirror-config",
            s(&p.cfg),
            "--interval",
            "100ms",
            "--for",
            "3s",
            "--json",
        ])
        .env("FAKE_PROFILE_DIR", &p.hermes)
        .env("XDG_CONFIG_HOME", p.t.0.join("xdg-config"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    // the same configuration plus a comment, swapped in unreadable (one rename, so watch never sees it readable)
    let text = std::fs::read_to_string(&p.cfg).unwrap();
    let next = p.cfg.with_extension("next");
    std::fs::write(&next, format!("{text}\n# demo note\n")).unwrap();
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::rename(&next, &p.cfg).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    // readable again, with the same stamp: only a retry picks it up
    std::fs::set_permissions(&p.cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
    let o = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
    assert_eq!(
        err.matches("still running on the previous configuration")
            .count(),
        1,
        "{err}"
    );
    assert!(v["syncs"].as_u64().unwrap_or(0) >= 1, "{v} {err}");
}

#[test]
fn check_runs_the_connection_round_trip() {
    let p = Pair::new("mirror-check");
    w(&p.export, &export_with("Example instructions.", &[]));
    p.box_digest();
    p.init();
    let run = || p.bp(&["mirror", "check"]);
    let stage = |r: (i32, serde_json::Value, String)| {
        (r.0, r.1["stage"].as_str().unwrap_or("").to_string())
    };
    // the round folder this machine just made under probe/
    let round = || {
        std::fs::read_dir(p.drive.join("probe"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .max_by_key(|e| e.metadata().unwrap().modified().unwrap())
            .unwrap()
            .path()
    };
    let read = |f: &str| std::fs::read_to_string(round().join(f)).unwrap();
    // the Grok side has not written its probe file yet
    assert_eq!(stage(run()), (3, "bot-file".into()));
    w(
        &p.drive.join("probe/bot.txt"),
        "botport probe from the bot\n",
    );
    assert_eq!(stage(run()), (3, "host-files".into()));
    assert_eq!(stage(run()), (3, "bot-ack".into()));
    // an answer created but not filled in yet is still waiting
    w(&round().join("ack-1.txt"), "");
    assert_eq!(stage(run()), (3, "bot-ack".into()));
    let (new, old) = (read("host-new.txt"), read("host-old.txt"));
    w(&round().join("ack-1.txt"), &format!("{new}{old}"));
    // this machine overwrites host-old.txt; a bot that hands back the old copy is caught
    assert_eq!(stage(run()), (3, "host-overwrite".into()));
    assert_ne!(read("host-old.txt"), old);
    assert_eq!(stage(run()), (3, "bot-ack-2".into()));
    w(&round().join("ack-2.txt"), &old);
    assert_eq!(stage(run()), (2, "bot-read-overwrite".into()));
    w(&round().join("ack-2.txt"), &read("host-old.txt"));
    assert_eq!(stage(run()), (0, "passed".into()));
    // a new round never reads the old round's answers, and never deletes a file the bot made
    let first = round();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let (c, v, _) = p.bp(&["mirror", "check", "--reset"]);
    assert_eq!((c, v["stage"].as_str()), (3, Some("host-files")));
    assert_ne!(round(), first);
    assert!(first.join("ack-1.txt").exists() && first.join("ack-2.txt").exists());
    assert_eq!(stage(run()), (3, "bot-ack".into()));
    // a bot that cannot read this machine's files
    w(&round().join("ack-1.txt"), "nothing it could read\n");
    assert_eq!(stage(run()), (2, "bot-read-new".into()));
}

#[test]
fn an_export_that_yields_nothing_is_refused_not_read_as_empty() {
    let p = Pair::new("mirror-empty-from");
    w(&p.export, &export_with("Example instructions.", &[]));
    // --from names the folder that holds the export file, not the file
    let folder = p.export.parent().unwrap();
    let out = p.drive.join("from-grok");
    let (c, _, e) = p.bp(&[
        "mirror",
        "digest-items",
        "--pair",
        "demo-bot",
        "--from",
        s(folder),
        "--out",
        s(&out),
    ]);
    assert_eq!(c, 2, "{e}");
    assert!(
        e.contains("no setup found") && e.contains("export file itself"),
        "{e}"
    );
    assert!(
        !out.exists() || std::fs::read_dir(&out).unwrap().next().is_none(),
        "nothing written"
    );
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
    // the target is one second; a loaded or throttled machine may raise the limit for a local run
    let limit = std::env::var("BOTPORT_QUIET_SYNC_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    assert!(
        ms < limit,
        "a quiet sync of 50 skills and 20 routines took {ms} ms (limit {limit} ms)"
    );
}

#[test]
fn a_mass_retire_writes_nothing_without_allow_retire_and_a_held_change_never_is_written() {
    let p = Pair::new("mirror-mass-retire");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 5, 0),
    );
    p.box_digest();
    p.init();
    let mut items = vec![
        "persona.name",
        "persona.instructions",
        "routine:demo-routine",
    ];
    let skills: Vec<String> = (0..5).map(|i| format!("skill:demo-skill-{i}")).collect();
    items.extend(skills.iter().map(String::as_str));
    for it in items {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    let skill = |i: usize| {
        p.hermes
            .join(format!("skills/mirror/demo-skill-{i}/SKILL.md"))
    };
    assert!((0..5).all(|i| skill(i).is_file()));
    // the Grok side drops all five skills (more than the guard allows) and changes the enabled routine's prompt
    w(
        &p.export,
        &export_json("Example instructions.", "Changed prompt.", 0, 0),
    );
    p.box_digest();
    let (c, v, e) = p.sync();
    assert_eq!(c, 2, "{v} {e}");
    assert!(e.contains("--allow-retire"), "{e}");
    assert!((0..5).all(|i| skill(i).is_file()), "nothing retired");
    assert_eq!(job(&jobs(&p), "demo-routine")["prompt"], "Example prompt.");
    // allowed: the skills go, the routine change still waits for the owner and is not written
    let (c, v, e) = p.bp(&["mirror", "sync", "--allow-retire"]);
    assert_eq!(
        (c, v["waiting_for_approval"].as_u64()),
        (3, Some(1)),
        "{v} {e}"
    );
    assert!((0..5).all(|i| !skill(i).exists()));
    assert_eq!(job(&jobs(&p), "demo-routine")["prompt"], "Example prompt.");
    let (c, v, e) = p.sync();
    assert_eq!(c, 3, "{v} {e}");
    assert_eq!(job(&jobs(&p), "demo-routine")["prompt"], "Example prompt.");
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
    // a connector only on the Hermes side, at a url the policy refuses to carry
    w(
        &p.hermes.join("config.yaml"),
        "mcp_servers:\n  demo-connector:\n    url: http://203.0.113.7/mcp\n",
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
    let (_, plan, _) = p.bp(&["mirror", "plan"]);
    let item = |id: &str| {
        plan["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["item"] == id)
            .cloned()
            .unwrap_or_default()
    };
    let conn = item("connector:demo-connector");
    assert_eq!(conn["action"], "unsupported", "{plan}");
    assert!(
        conn["why"].as_str().unwrap().contains("url refused"),
        "{conn}"
    );
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

#[test]
fn the_first_init_hands_the_owner_the_briefs_and_the_message_to_send() {
    let p = Pair::new("mirror-first-setup");
    let (c, _, e) = init_with(&p, &[]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("a pair key was made"), "{e}");
    // the first setup is the owner's own request to their own Mirror bot, never an automation's to a bot
    assert!(
        e.contains(
            "first setup: if your account has no Mirror bot yet, create one in the app yourself"
        ),
        "{e}"
    );
    assert!(
        e.contains("Please add the pair demo-bot for my bot <its name>"),
        "{e}"
    );
    let dir = p.cfg.with_extension("brief");
    assert!(!dir.join("AGENT-EXPORT.md").exists());
    for (name, doc) in [
        ("AGENT-MIRROR.md", "docs/AGENT-MIRROR.md"),
        ("MIRROR-BOT.md", "docs/MIRROR-BOT.md"),
    ] {
        let f = dir.join(name);
        assert!(e.contains(s(&f)), "{e}");
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(doc)).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&f).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }
    // once the bot has written its first state file, init goes ahead
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    p.box_digest();
    let (c, v, e) = init_with(&p, &[]);
    assert_eq!(c, 0, "{v} {e}");
}

#[test]
fn a_bot_that_keeps_its_instructions_still_mirrors_the_rest() {
    let p = Pair::new("mirror-without-instructions");
    let digest_without = |p: &Pair| {
        let (c, v, e) = p.bp(&[
            "mirror",
            "digest-items",
            "--pair",
            "demo-bot",
            "--from",
            s(&p.export),
            "--out",
            s(&p.drive.join("from-grok")),
            "--log",
            s(&p.t.0.join("box/applied.jsonl")),
            "--without",
            "instructions",
        ]);
        assert_eq!(c, 0, "{e}");
        v
    };
    // whatever the export holds for the instructions is dropped and reported as not mirrored
    w(
        &p.export,
        &export_json("Example Grok instructions.", "Example prompt.", 1, 0),
    );
    let (c, _, e) = init_with(&p, &[]);
    assert_eq!(c, 2, "{e}");
    let v = digest_without(&p);
    assert!(
        v["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u == "persona.instructions"),
        "{v}"
    );
    let state = std::fs::read_dir(p.drive.join("from-grok/blobs"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap_or_default())
        .collect::<String>();
    assert!(!state.contains("Example Grok instructions"));
    // the Hermes side keeps its own instructions
    let own = "Owner text.\n<!-- botport:begin persona -->\nExample Hermes instructions.\n<!-- botport:end persona -->\n";
    w(&p.hermes.join("SOUL.md"), own);
    let (c, v, e) = init_with(&p, &[]);
    assert_eq!(c, 0, "{v} {e}");
    // an item that is not mirrored is not put on the pick-one list
    assert_eq!(v["to_pick"], 3, "{v}");
    for it in ["persona.name", "skill:demo-skill-0", "routine:demo-routine"] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    let items = v["items"].as_array().unwrap();
    let ins = items
        .iter()
        .find(|i| i["item"] == "persona.instructions")
        .unwrap_or_else(|| panic!("{v} {e}"));
    assert_eq!(ins["action"], "unsupported", "{v}");
    assert!(
        ins["why"]
            .as_str()
            .unwrap()
            .contains("not mirrored (the bot does not share it)"),
        "{v}"
    );
    // the rest of the setup still arrives
    assert!(p
        .hermes
        .join("skills/mirror/demo-skill-0/SKILL.md")
        .is_file());
    // an edit on the Hermes side is not sent: the bot keeps its own instructions
    w(
        &p.hermes.join("SOUL.md"),
        &soul(&p.hermes).replace("Example Hermes", "Edited Hermes"),
    );
    let (_, v, e) = p.sync();
    assert!(v["gen"].is_null(), "nothing published: {v} {e}");
    assert!(soul(&p.hermes).contains("Edited Hermes instructions."));
    let (c, _, e) = p.bp(&[
        "mirror",
        "digest-items",
        "--pair",
        "demo-bot",
        "--from",
        s(&p.export),
        "--out",
        s(&p.t.0.join("box/out")),
        "--without",
        "memories",
    ]);
    assert_eq!(c, 2);
    assert!(e.contains("--without takes only: instructions"), "{e}");
}

/// A box command run as a coordinator would: no pair flags, everything from the root's layout.
fn boxcmd(state: &Path, args: &[&str]) -> (i32, serde_json::Value, String) {
    let o = bin(state).args(args).arg("--json").output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null),
        String::from_utf8_lossy(&o.stderr).to_string(),
    )
}

#[test]
fn a_bot_that_stops_sharing_its_instructions_withdraws_a_send_in_flight() {
    let p = Pair::new("mirror-opt-out");
    let base: String = (1..=8)
        .map(|i| format!("Example instruction line {i}.\n"))
        .collect();
    w(
        &p.export,
        &export_json(base.trim_end(), "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    for it in [
        "persona.name",
        "persona.instructions",
        "skill:demo-skill-0",
        "routine:demo-routine",
    ] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    // a Hermes edit to the instructions is on its way to the Grok side
    w(
        &p.hermes.join("SOUL.md"),
        &soul(&p.hermes).replace("line 1.", "line 1, edited on Hermes."),
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["gen"].as_u64()), (0, Some(1)), "{v} {e}");
    // the bot now keeps its instructions: the staged edit is held, not pending, and the check passes
    let stage = p.t.0.join("box/stage");
    let apply = |extra: &[&str]| {
        let mut a = vec![
            "mirror",
            "apply-pack",
            "--pair",
            "demo-bot",
            "--dir",
            s(&p.drive),
            "--from",
            s(&p.export),
            "--stage",
            s(&stage),
            "--without",
            "instructions",
        ];
        a.extend(extra);
        p.bp(&a)
    };
    let (c, v, e) = apply(&["--check"]);
    assert_eq!((c, v["pending"].as_u64()), (0, Some(0)), "{v} {e}");
    assert!(!p.t.0.join("box/stage/persona.instructions.txt").exists());
    let (c, _, e) = p.bp(&[
        "mirror",
        "digest-items",
        "--pair",
        "demo-bot",
        "--from",
        s(&p.export),
        "--out",
        s(&p.drive.join("from-grok")),
        "--without",
        "instructions",
    ]);
    assert_eq!(c, 0, "{e}");
    // the host withdraws the send and keeps its own copy
    let (c, v, e) = p.sync();
    assert_eq!((c, v["in_flight"].as_u64()), (0, Some(0)), "{v} {e}");
    assert!(soul(&p.hermes).contains("line 1, edited on Hermes."));
    let (_, v, e) = p.bp(&["mirror", "status"]);
    assert!(
        v["in_flight"].as_array().is_some_and(|a| a.is_empty()),
        "{v} {e}"
    );
    // the bot shares its (unchanged, older) instructions again: the copies differ, so the Hermes edit is sent again,
    // never taken as in step
    p.box_digest();
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert!(v["gen"].as_u64().is_some(), "a new pack: {v} {e}");
    let v = p.box_apply();
    assert_eq!(v["pending"], 1, "{v}");
    let staged = std::fs::read_to_string(p.t.0.join("box/stage/persona.instructions.txt")).unwrap();
    assert!(staged.contains("line 1, edited on Hermes."));
}

#[test]
fn a_coordinator_runs_the_box_steps_for_several_pairs() {
    let t = Tmp::new("mirror-coordinator");
    let root = t.0.join("box-root");
    let work = |pair: &str| root.join(".work").join(pair);
    for (pair, n) in [("demo-a", 1), ("demo-b", 2)] {
        w(
            &work(pair).join("export"),
            &export_json("Example instructions.", "Example prompt.", n, 0),
        );
        // these pairs are unsigned on purpose; signing through a root is tested end to end below
        w(&work(pair).join("unsigned"), "");
    }
    let digest = |extra: &[&str]| {
        let mut a = vec!["mirror", "digest-items", "--root", s(&root)];
        a.extend(extra);
        boxcmd(&t.0, &a)
    };
    let (c, v, e) = digest(&[]);
    assert_eq!(c, 0, "{v} {e}");
    for pair in ["demo-a", "demo-b"] {
        assert_eq!(v["pairs"][pair]["n"], 1, "{v}");
        assert_eq!(v["pairs"][pair]["unchanged"], false, "{v}");
        assert!(root.join(pair).join("from-grok/state-1.json").is_file());
        // the bot uploaded it and says so
        w(&work(pair).join("uploaded"), "1\n");
    }
    // nothing changed since the uploaded file: nothing is written, so a frequent routine has nothing to upload
    let (c, v, e) = digest(&[]);
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["pairs"]["demo-a"]["unchanged"], true, "{v}");
    assert!(!root.join("demo-a/from-grok/state-2.json").exists());
    // one bot's setup changes: only its pair gets a new state file
    w(
        &work("demo-b").join("export"),
        &export_json("Example instructions.", "Example prompt, changed.", 2, 0),
    );
    let (c, v, e) = digest(&[]);
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(
        (
            v["pairs"]["demo-a"]["unchanged"].as_bool(),
            v["pairs"]["demo-b"]["n"].as_u64()
        ),
        (Some(true), Some(2)),
        "{v}"
    );
    // state-2 never got uploaded (the run was cut short): the next run writes a new file with every blob again
    let blobs = root.join("demo-b/from-grok/blobs");
    for e in std::fs::read_dir(&blobs).unwrap() {
        std::fs::remove_file(e.unwrap().path()).unwrap();
    }
    let (c, v, e) = digest(&["--pair", "demo-b"]);
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["pairs"]["demo-b"]["n"], 3, "{v}");
    let st: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("demo-b/from-grok/state-3.json")).unwrap())
            .unwrap();
    let items = st["items"].as_object().unwrap();
    let named: Vec<&str> = items.values().filter_map(|e| e["blob"].as_str()).collect();
    assert_eq!(named.len(), items.len(), "{st}");
    assert!(named.iter().all(|b| blobs.join(b).is_file()), "{st}");
    // --pair narrows the run
    assert!(v["pairs"].get("demo-a").is_none(), "{v}");
    w(&work("demo-b").join("uploaded"), "3\n");
    let (_, v, _) = digest(&["--pair", "demo-b"]);
    assert_eq!(v["pairs"]["demo-b"]["unchanged"], true, "{v}");
    // one broken pair does not stop the others
    w(&work("demo-c").join("export"), "not an export\n");
    w(&work("demo-c").join("unsigned"), "");
    let (c, v, _) = digest(&[]);
    assert_eq!(c, 2, "{v}");
    assert!(v["pairs"]["demo-c"]["error"].is_string(), "{v}");
    assert_eq!(v["pairs"]["demo-b"]["unchanged"], true, "{v}");
    // a work folder that is a link is never followed
    #[cfg(unix)]
    {
        let elsewhere = t.0.join("elsewhere");
        w(
            &elsewhere.join("export"),
            &export_json("Example instructions.", "Example prompt.", 1, 0),
        );
        std::os::unix::fs::symlink(&elsewhere, work("demo-d")).unwrap();
        let (_, v, _) = digest(&[]);
        assert!(v["pairs"].get("demo-d").is_none(), "{v}");
    }
    // the root sets every path itself, and the upload record is per pair
    let (c, _, e) = digest(&["--out", s(&t.0.join("x"))]);
    assert_eq!(c, 2);
    assert!(
        e.contains("--root sets every path itself: drop --out"),
        "{e}"
    );
    let (c, v, _) = digest(&["--uploaded", "1"]);
    assert_eq!(c, 2);
    assert!(
        v["pairs"]["demo-a"]["error"]
            .as_str()
            .unwrap()
            .contains("--uploaded is per pair"),
        "{v}"
    );
    // a pair that has had nothing sent yet has no pack to stage
    let (c, v, e) = boxcmd(
        &t.0,
        &[
            "mirror",
            "apply-pack",
            "--root",
            s(&root),
            "--pair",
            "demo-a",
        ],
    );
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["pairs"]["demo-a"]["pending"], 0, "{v}");
}

#[test]
fn a_coordinator_root_serves_a_signed_pair_end_to_end() {
    let p = Pair::new("mirror-coordinator-e2e");
    let root = p.t.0.join("coord");
    let folder = root.join("demo-bot");
    std::fs::create_dir_all(&folder).unwrap();
    let init = || {
        p.bp(&[
            "mirror",
            "init",
            "--profile-dir",
            s(&p.hermes),
            "--hermes-bin",
            s(&p.fake),
            "--transport",
            "drive",
            "--drive-dir",
            s(&folder),
            "--metrics",
            s(&p.t.0.join("m.csv")),
        ])
    };
    let (c, _, e) = init();
    assert_eq!(c, 2, "{e}");
    // the coordinator holds the pair key and the bot's export in the root's layout
    let work = root.join(".work/demo-bot");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::copy(p.cfg.with_extension("key"), work.join("pair.key")).unwrap();
    let base: String = (1..=8)
        .map(|i| format!("Example instruction line {i}.\n"))
        .collect();
    w(
        &work.join("export"),
        &export_json(base.trim_end(), "Example prompt.", 1, 0),
    );
    // complete coverage: a program read of the bot's setup; every written file counts as uploaded here
    let digest = || {
        let r = boxcmd(
            &p.t.0,
            &[
                "mirror",
                "digest-items",
                "--root",
                s(&root),
                "--coverage",
                "complete",
            ],
        );
        if let Some(n) = r.1["pairs"]["demo-bot"]["n"].as_u64() {
            w(&work.join("uploaded"), &format!("{n}\n"));
        }
        r
    };
    let (c, v, e) = digest();
    assert_eq!(c, 0, "{v} {e}");
    let (c, v, e) = init();
    assert_eq!(c, 0, "signed state file accepted: {v} {e}");
    for it in [
        "persona.name",
        "persona.instructions",
        "skill:demo-skill-0",
        "routine:demo-routine",
    ] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    // a Hermes-side edit reaches the coordinator's stage for that pair
    w(
        &p.hermes.join("SOUL.md"),
        &soul(&p.hermes).replace("line 1.", "line 1, edited on Hermes."),
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["gen"].as_u64()), (0, Some(1)), "{v} {e}");
    let (c, v, e) = boxcmd(&p.t.0, &["mirror", "apply-pack", "--root", s(&root)]);
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["pairs"]["demo-bot"]["pending"], 1, "{v}");
    let staged = std::fs::read_to_string(work.join("stage/persona.instructions.txt")).unwrap();
    assert!(staged.contains("edited on Hermes"));
    // the bot installs it; the coordinator's next run logs it and writes a new state file
    w(
        &work.join("export"),
        &export_json(staged.trim_end(), "Example prompt.", 1, 0),
    );
    let (c, v, e) = boxcmd(
        &p.t.0,
        &["mirror", "apply-pack", "--root", s(&root), "--check"],
    );
    assert_eq!(c, 0, "{v} {e}");
    let (c, v, e) = digest();
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["pairs"]["demo-bot"]["unchanged"], false, "{v}");
    let (c, v, e) = p.sync();
    assert_eq!((c, v["in_flight"].as_u64()), (0, Some(0)), "{v} {e}");
    // and with nothing new, the next run writes nothing
    let (c, v, e) = digest();
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["pairs"]["demo-bot"]["unchanged"], true, "{v}");
    // a signed pair whose key file went missing is refused, never written unsigned
    std::fs::remove_file(work.join("pair.key")).unwrap();
    let (c, v, _) = digest();
    assert_eq!(c, 2, "a signed pair without its key is refused: {v}");
    assert!(
        v["pairs"]["demo-bot"]["error"]
            .as_str()
            .unwrap()
            .contains("no pair key at"),
        "{v}"
    );
}

#[test]
fn a_coordinator_root_refuses_what_would_weaken_a_pair() {
    let t = Tmp::new("mirror-coordinator-refusals");
    let root = t.0.join("box-root");
    let work = root.join(".work/demo-a");
    w(
        &work.join("export"),
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    // a missing key is an error for that pair, unless the pair is marked unsigned on purpose
    let (c, v, _) = boxcmd(&t.0, &["mirror", "digest-items", "--root", s(&root)]);
    assert_eq!(c, 2, "{v}");
    assert!(
        v["pairs"]["demo-a"]["error"]
            .as_str()
            .unwrap()
            .contains("no pair key at"),
        "{v}"
    );
    assert!(!root.join("demo-a/from-grok").exists());
    w(&work.join("unsigned"), "");
    let (c, v, e) = boxcmd(&t.0, &["mirror", "digest-items", "--root", s(&root)]);
    assert_eq!(c, 0, "{v} {e}");
    // a precheck with no pack in the pair folder fails, never passes
    let (c, v, _) = boxcmd(
        &t.0,
        &[
            "mirror",
            "apply-pack",
            "--root",
            s(&root),
            "--pair",
            "demo-a",
            "--precheck",
            "skill:demo-skill-0",
            "--gen",
            "1",
        ],
    );
    assert_eq!(c, 2, "{v}");
    assert!(v["pairs"]["demo-a"]["error"].is_string(), "{v}");
    // a pair named twice runs once
    let (c, v, e) = boxcmd(
        &t.0,
        &[
            "mirror",
            "digest-items",
            "--root",
            s(&root),
            "--pair",
            "demo-a",
            "--pair",
            "demo-a",
        ],
    );
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["pairs"]["demo-a"]["n"], 2, "{v}");
    assert!(!root.join("demo-a/from-grok/state-3.json").exists());
    // an unreadable upload record counts as no upload: a new file with every blob
    w(&work.join("uploaded"), "two\n");
    let (c, v, e) = boxcmd(&t.0, &["mirror", "digest-items", "--root", s(&root)]);
    assert_eq!(
        (c, v["pairs"]["demo-a"]["n"].as_u64()),
        (0, Some(3)),
        "{v} {e}"
    );
    // a record newer than any file here: the copy is restored first, never numbered below what was uploaded
    w(&work.join("uploaded"), "9\n");
    let (c, v, _) = boxcmd(&t.0, &["mirror", "digest-items", "--root", s(&root)]);
    assert_eq!(c, 2, "{v}");
    assert!(
        v["pairs"]["demo-a"]["error"]
            .as_str()
            .unwrap()
            .contains("restore it from the pair's Drive folder"),
        "{v}"
    );
    std::fs::remove_file(work.join("uploaded")).unwrap();
    // a linked export is refused, never followed
    #[cfg(unix)]
    {
        let x = root.join(".work/demo-x");
        std::fs::create_dir_all(&x).unwrap();
        w(&x.join("unsigned"), "");
        let real = t.0.join("real-export.json");
        w(
            &real,
            &export_json("Example instructions.", "Example prompt.", 1, 0),
        );
        std::os::unix::fs::symlink(&real, x.join("export")).unwrap();
        let (c, v, _) = boxcmd(
            &t.0,
            &[
                "mirror",
                "digest-items",
                "--root",
                s(&root),
                "--pair",
                "demo-x",
            ],
        );
        assert_eq!(c, 2, "{v}");
        assert!(
            v["pairs"]["demo-x"]["error"]
                .as_str()
                .unwrap()
                .contains("is a link"),
            "{v}"
        );
    }
    // a linked pair folder is refused, never followed
    #[cfg(unix)]
    {
        let b = root.join(".work/demo-b");
        w(
            &b.join("export"),
            &export_json("Example instructions.", "Example prompt.", 1, 0),
        );
        w(&b.join("unsigned"), "");
        let elsewhere = t.0.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("demo-b")).unwrap();
        let (c, v, _) = boxcmd(&t.0, &["mirror", "digest-items", "--root", s(&root)]);
        assert_eq!(c, 2, "{v}");
        assert!(
            v["pairs"]["demo-b"]["error"]
                .as_str()
                .unwrap()
                .contains("a link is never followed"),
            "{v}"
        );
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }
}

#[test]
fn a_pack_from_another_release_is_republished() {
    use botport::adapters::mirror::pack::{Head, Pack};
    let p = Pair::new("mirror-old-form-pack");
    let base: String = (1..=8)
        .map(|i| format!("Example instruction line {i}.\n"))
        .collect();
    w(
        &p.export,
        &export_json(base.trim_end(), "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    for it in [
        "persona.name",
        "persona.instructions",
        "skill:demo-skill-0",
        "routine:demo-routine",
    ] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    w(
        &p.hermes.join("SOUL.md"),
        &soul(&p.hermes).replace("line 1.", "line 1, edited on Hermes."),
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["gen"].as_u64()), (0, Some(1)), "{v} {e}");
    // HEAD's pack is rewritten as an older release would have made it (signed, intact, another neutral form)
    let key = botport::core::auth::parse_key(
        &std::fs::read_to_string(p.cfg.with_extension("key")).unwrap(),
    )
    .unwrap();
    let head = Head::parse(&std::fs::read(p.drive.join("HEAD")).unwrap()).unwrap();
    let path = p.drive.join("to-grok").join(&head.pack);
    let mut old = Pack::parse(&std::fs::read(&path).unwrap(), "demo-bot").unwrap();
    old.neutral_form = 1;
    let bytes = old.seal_with(Some(&key));
    std::fs::write(&path, &bytes).unwrap();
    let mut h = old.head();
    h.pack = head.pack.clone();
    std::fs::write(p.drive.join("HEAD"), h.to_line()).unwrap();
    // the box says what is wrong instead of staging from it
    let (c, _, e) = p.bp(&[
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
    ]);
    assert_eq!(c, 2);
    assert!(e.contains("run the same botport release"), "{e}");
    // with nothing new to send, the host still publishes a pack the box can read
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert!(v["gen"].as_u64().is_some_and(|g| g > 1), "{v} {e}");
    let v = p.box_apply();
    assert_eq!(v["pending"], 1, "{v}");
}

#[test]
fn approval_is_the_default() {
    let p = Pair::new("mirror-approval-default");
    let base: String = (1..=8)
        .map(|i| format!("Example instruction line {i}.\n"))
        .collect();
    w(
        &p.export,
        &export_json(base.trim_end(), "Example prompt.", 1, 0),
    );
    p.box_digest();
    let (c, v, e) = p.init_signed(&["--default-approval"]);
    assert_eq!(c, 0, "{v} {e}");
    let cfg = std::fs::read_to_string(&p.cfg).unwrap();
    assert!(cfg.contains("approve = \"all\""), "{cfg}");
    for it in [
        "persona.name",
        "persona.instructions",
        "skill:demo-skill-0",
        "routine:demo-routine",
    ] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    let (_, _, _) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    // a small instruction edit on the Grok side waits for the owner instead of being carried
    w(
        &p.export,
        &export_json(
            &base
                .trim_end()
                .replace("line 1.", "line 1, edited on Grok."),
            "Example prompt.",
            1,
            0,
        ),
    );
    p.box_digest();
    let (c, v, e) = p.sync();
    assert_eq!(
        (c, v["waiting_for_approval"].as_u64()),
        (3, Some(1)),
        "{v} {e}"
    );
    assert!(!soul(&p.hermes).contains("edited on Grok"));
    let (c, _, e) = p.bp(&["mirror", "approve", "persona.instructions", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    assert!(soul(&p.hermes).contains("edited on Grok"));
}

#[test]
fn a_hermes_without_cron_timezones_gets_utc_routines_and_a_clear_refusal() {
    let p = Pair::new("mirror-no-cron-tz");
    // a Hermes whose `cron create` has no --timezone option
    let fake = FAKE_HERMES.replacen(
        "a = sys.argv[1:]\n",
        "a = sys.argv[1:]\nif \"--help\" in a:\n    print(\"usage: hermes cron create [--name NAME] [--deliver D] [--paused]\"); sys.exit(0)\nif any(x.startswith(\"--timezone\") for x in a):\n    print(\"hermes: error: unrecognized arguments: --timezone\", file=sys.stderr); sys.exit(2)\n",
        1,
    );
    w(&p.fake, &fake);
    let mut v: serde_json::Value = serde_json::from_str(&export_json(
        "Example instructions.",
        "Example prompt.",
        0,
        2,
    ))
    .unwrap();
    // one routine in UTC (as an export without a zone reads), one in another zone
    let routines = v["bots"][0]["routines"].as_array_mut().unwrap();
    routines[1]["timezone"] = serde_json::json!("Europe/Berlin");
    w(&p.export, &v.to_string());
    p.box_digest();
    p.init();
    for it in ["routine:demo-routine-0", "routine:demo-routine-1"] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        if it.ends_with('1') {
            assert_ne!(c, 0);
            assert!(
                e.contains("this Hermes cannot store a routine timezone (Europe/Berlin)"),
                "{e}"
            );
        } else {
            assert_eq!(c, 0, "{it}: {e}");
        }
    }
    let jobs = std::fs::read_to_string(p.hermes.join("cron/jobs.json")).unwrap();
    assert!(
        jobs.contains("demo-routine-0") && !jobs.contains("demo-routine-1"),
        "{jobs}"
    );
    assert!(jobs.contains("\"timezone\": null"), "{jobs}");
}

#[test]
fn an_item_made_local_only_while_in_flight_is_withdrawn_from_the_pack() {
    let p = Pair::new("mirror-local-only-in-flight");
    let base: String = (1..=8)
        .map(|i| format!("Example instruction line {i}.\n"))
        .collect();
    w(
        &p.export,
        &export_json(base.trim_end(), "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    for it in [
        "persona.name",
        "persona.instructions",
        "skill:demo-skill-0",
        "routine:demo-routine",
    ] {
        let (c, _, e) = p.bp(&["mirror", "pick", it, "--keep", "grok", "--yes"]);
        assert_eq!(c, 0, "{it}: {e}");
    }
    let (c, _, e) = p.bp(&["mirror", "approve", "routine:demo-routine", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    // a Hermes edit to the instructions is on its way to the Grok side
    w(
        &p.hermes.join("SOUL.md"),
        &soul(&p.hermes).replace("line 1.", "line 1, edited on Hermes."),
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["gen"].as_u64()), (0, Some(1)), "{v} {e}");
    // the owner keeps the instructions as they are on each side: the send is withdrawn
    let (c, v, e) = p.bp(&[
        "mirror",
        "pick",
        "persona.instructions",
        "--local-only",
        "--yes",
    ]);
    assert_eq!(
        (c, v["send_withdrawn"].as_bool()),
        (0, Some(true)),
        "{v} {e}"
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["in_flight"].as_u64()), (0, Some(0)), "{v} {e}");
    // the pack HEAD names now leaves the instructions out
    let stage = p.t.0.join("box/stage");
    let (c, v, e) = p.bp(&[
        "mirror",
        "apply-pack",
        "--pair",
        "demo-bot",
        "--dir",
        s(&p.drive),
        "--from",
        s(&p.export),
        "--stage",
        s(&stage),
        "--check",
    ]);
    assert_eq!((c, v["pending"].as_u64()), (0, Some(0)), "{v} {e}");
    assert!(!stage.join("persona.instructions.txt").exists());
    assert!(soul(&p.hermes).contains("line 1, edited on Hermes."));
    // mirrored again: the copies differ, so the Hermes edit is sent again, never taken as in step
    let cfg = std::fs::read_to_string(&p.cfg).unwrap();
    w(
        &p.cfg,
        &cfg.replace("local_only = [\"persona.instructions\"]\n", ""),
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["in_flight"].as_u64()), (0, Some(1)), "{v} {e}");
}

#[test]
fn an_item_kept_local_only_leaves_the_conflicts_and_is_never_mirrored() {
    let p = Pair::new("mirror-local-only");
    w(
        &p.export,
        &export_with(
            "Example instructions.",
            &[("Example routine", "Example prompt.", false)],
        ),
    );
    p.box_digest_with("partial");
    let v = p.init();
    assert_eq!(v["to_pick"], 3, "{v}");
    // the owner keeps the routine as it is on each side; the conflict goes, the config lists it
    let (c, v, e) = p.bp(&[
        "mirror",
        "pick",
        "routine:Example routine",
        "--local-only",
        "--yes",
    ]);
    assert_eq!(c, 0, "{v} {e}");
    assert_eq!(v["conflict_cleared"], true, "{v}");
    assert!(std::fs::read_to_string(&p.cfg)
        .unwrap()
        .contains("local_only = [\"routine:Example routine\"]"));
    let (_, v, e) = p.sync();
    let items = v["items"].as_array().unwrap();
    let r = items
        .iter()
        .find(|i| i["item"] == "routine:Example routine")
        .unwrap_or_else(|| panic!("{v} {e}"));
    assert_eq!(r["action"], "unsupported", "{v}");
    assert!(r["why"].as_str().unwrap().contains("local-only"), "{v}");
    assert!(!std::fs::read_to_string(p.hermes.join("cron/jobs.json"))
        .unwrap_or_default()
        .contains("Example routine"));
    // an item neither side has now: its conflict can be forgotten, never one that a side still has
    let (c, _, e) = p.bp(&[
        "mirror",
        "pick",
        "persona.instructions",
        "--forget",
        "--yes",
    ]);
    assert_ne!(c, 0);
    assert!(e.contains("one side has it"), "{e}");
    w(&p.export, &export_with("", &[]));
    p.box_digest_with("partial");
    let (c, v, e) = p.bp(&[
        "mirror",
        "pick",
        "persona.instructions",
        "--forget",
        "--yes",
    ]);
    assert_eq!((c, v["forgotten"].as_bool()), (0, Some(true)), "{v} {e}");
    // the Grok side is read from its export: botport cannot see that the item is gone there, and says so
    assert_eq!(v["unseen_on"], serde_json::json!(["Grok"]), "{v}");
}

#[test]
fn no_botport_text_for_a_bot_asks_it_to_export_itself() {
    let p = Pair::new("mirror-no-self-export");
    let (_, _, said) = init_with(&p, &[]);
    let (c, v, e) = p.bp(&["mirror", "setup", "--print-charter"]);
    assert_eq!(c, 0, "{e}");
    let charter = v["charter"].as_str().unwrap().to_string();
    assert_eq!(
        charter,
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/MIRROR-BOT.md"))
            .unwrap()
    );
    let dir = p.cfg.with_extension("brief");
    let mut texts = vec![("init", said), ("charter", charter)];
    for f in ["AGENT-MIRROR.md", "MIRROR-BOT.md"] {
        texts.push((f, std::fs::read_to_string(dir.join(f)).unwrap()));
    }
    // and the export brief's step for other bots messages none of them
    let export =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/AGENT-EXPORT.md"))
            .unwrap();
    let step7 = export[export.find("## Step 7").unwrap()..].to_string();
    texts.push(("AGENT-EXPORT.md step 7", step7));
    for (what, t) in &texts {
        let t = t.to_lowercase();
        for asks in [
            "export yourself",
            "export your setup",
            "export your own",
            "describe yourself",
            "please do steps",
            "send it this file",
            "for yourself",
            "set up botport mirror mode",
        ] {
            assert!(!t.contains(asks), "{what} asks a bot: {asks:?}");
        }
    }
}

#[test]
fn a_bot_read_from_the_box_files_mirrors_its_name_and_names_the_rest() {
    let p = Pair::new("mirror-box-read");
    let data = p.t.0.join("box/agent-data");
    let id = "0a1b2c3d-0000-4000-8000-00000000abcd";
    w(
        &data.join(format!("agents/{id}/profile.json")),
        r#"{"name": "Demo Bot"}"#,
    );
    w(
        &data.join("workflows/demo-skill-0/SKILL.md"),
        "# Demo skill 0\nExample steps.",
    );
    // a routine on the Hermes side: the box files cannot show the Grok side's routines
    w(
        &p.hermes.join("cron/jobs.json"),
        r#"{"jobs": [{"id": "j1", "name": "demo-routine", "prompt": "Example prompt.", "schedule": {"expr": "0 9 * * 1"},
                     "enabled": true, "timezone": null, "paused_reason": null}]}"#,
    );
    let digest = |extra: &[&str]| {
        let out = p.drive.join("from-grok");
        let mut a = vec![
            "mirror",
            "digest-items",
            "--pair",
            "demo-bot",
            "--agent-id",
            id,
            "--agent-data",
            s(&data),
            "--out",
            s(&out),
        ];
        a.extend(extra);
        p.bp(&a)
    };
    let (c, v, e) = digest(&[]);
    assert_eq!(c, 0, "{v} {e}");
    assert!(
        v["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "persona.instructions"),
        "{v}"
    );
    // a box read never claims to be the whole setup
    let (c, _, e) = digest(&["--coverage", "complete"]);
    assert_ne!(c, 0);
    assert!(
        e.contains("the box files do not hold the whole setup"),
        "{e}"
    );
    // --from and --agent-id together are refused
    let (c, _, _) = digest(&["--from", s(&p.export)]);
    assert_ne!(c, 0);
    let r = init_with(&p, &[]);
    if r.0 == 2 && r.2.contains("a pair key was made") {
        let (c, _, e) = digest(&[]);
        assert_eq!(c, 0, "{e}");
    }
    // only the name is a pick: a kind the Grok side cannot read is never on the list
    let (c, v, e) = init_with(&p, &[]);
    assert_eq!((c, v["to_pick"].as_u64()), (0, Some(1)), "{v} {e}");
    let (c, _, e) = p.bp(&[
        "mirror",
        "pick",
        "routine:demo-routine",
        "--keep",
        "hermes",
        "--yes",
    ]);
    assert_ne!(c, 0);
    assert!(e.contains("has no open conflict"), "{e}");
    let (c, _, e) = p.bp(&["mirror", "pick", "persona.name", "--keep", "grok", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!((c, v["in_flight"].as_u64()), (0, Some(0)), "{v} {e}");
    let items = v["items"].as_array().unwrap_or_else(|| panic!("{v} {e}"));
    let find = |id: &str| items.iter().find(|i| i["item"] == id).cloned();
    assert!(find("skill:demo-skill-0").is_none(), "{v}");
    let r = find("routine:demo-routine").unwrap_or_else(|| panic!("{v}"));
    assert_eq!(r["action"], "unsupported", "{v}");
    assert!(
        r["why"].as_str().unwrap().contains("not in the box files"),
        "{v}"
    );
    // the routine stays on the Hermes side; status names every gap
    assert!(std::fs::read_to_string(p.hermes.join("cron/jobs.json"))
        .unwrap()
        .contains("demo-routine"));
    let (_, v, e) = p.bp(&["mirror", "status"]);
    assert!(
        v["grok_unread"]["routine"]
            .as_str()
            .is_some_and(|w| w.contains("not in the box files")),
        "{v} {e}"
    );
    assert!(v["grok_unread"]["connector"].is_string(), "{v}");
    assert!(
        v["grok_unread"]["skill"]
            .as_str()
            .is_some_and(|w| w.contains("shared by every bot in the box")),
        "{v}"
    );
    // a new Hermes routine cannot reach a box that cannot read routines: it goes on the list to apply by hand
    w(
        &p.hermes.join("cron/jobs.json"),
        r#"{"jobs": [{"id": "j1", "name": "demo-routine", "prompt": "Example prompt.", "schedule": {"expr": "0 9 * * 1"},
                     "enabled": true, "timezone": null, "paused_reason": null},
                    {"id": "j2", "name": "demo-routine-2", "prompt": "Example prompt 2.", "schedule": {"expr": "0 10 * * 1"},
                     "enabled": true, "timezone": null, "paused_reason": null}]}"#,
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["in_flight"].as_u64()), (0, Some(0)), "{v} {e}");
    let (_, v, e) = p.bp(&["mirror", "status"]);
    let listed = |v: &serde_json::Value| -> Vec<String> {
        v["to_apply"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    assert_eq!(listed(&v), ["routine:demo-routine-2"], "{v} {e}");
    let (c, _, e) = p.bp(&["mirror", "applied", "routine:demo-routine-2"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, _) = p.bp(&["mirror", "applied", "routine:demo-routine-2"]);
    assert_ne!(c, 0, "no longer listed");
    // the name is read from the box, never written there: a Hermes-side rename is listed, not sent
    let soul_text = soul(&p.hermes);
    assert!(soul_text.contains("Demo Bot"), "{soul_text}");
    w(
        &p.hermes.join("SOUL.md"),
        &soul_text.replace("Demo Bot", "Demo Bot Renamed"),
    );
    let (c, v, e) = p.sync();
    assert_eq!((c, v["in_flight"].as_u64()), (0, Some(0)), "{v} {e}");
    let (_, v, e) = p.bp(&["mirror", "status"]);
    assert_eq!(listed(&v), ["persona.name"], "{v} {e}");
    let (c, _, e) = p.bp(&[
        "mirror",
        "pick",
        "persona.name",
        "--keep",
        "hermes",
        "--yes",
    ]);
    assert_ne!(c, 0, "{e}");
    // the owner renames the bot in the app: both sides agree, the entry goes by itself
    w(
        &data.join(format!("agents/{id}/profile.json")),
        r#"{"name": "Demo Bot Renamed"}"#,
    );
    let (c, _, e) = digest(&[]);
    assert_eq!(c, 0, "{e}");
    let (c, v, e) = p.sync();
    assert_eq!(c, 0, "{v} {e}");
    let (_, v, e) = p.bp(&["mirror", "status"]);
    assert!(listed(&v).is_empty(), "{v} {e}");
}

#[test]
fn the_box_writes_each_pairs_signed_hermes_digest_for_the_recent_skill() {
    let p = Pair::new("mirror-recent");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    // the host's digest goes to the pair folder, signed with the pair key
    let (c, _, e) = p.bp(&["digest", "write", "--profile-dir", s(&p.hermes)]);
    assert_eq!(c, 0, "{e}");
    p.sync();
    let key = p.cfg.with_extension("key");
    let recent = |k: &Path| {
        p.bp(&[
            "mirror",
            "recent",
            "--pair",
            "demo-bot",
            "--dir",
            s(&p.drive),
            "--key",
            s(k),
        ])
    };
    let (c, v, e) = recent(&key);
    assert_eq!(c, 0, "{v} {e}");
    assert!(v["pairs"][0]["digest"].is_u64(), "{v}");
    let out = std::fs::read_to_string(p.drive.join("from-hermes/digest")).unwrap();
    assert!(out.contains("botport-digest/1"), "{out}");
    // a digest that does not verify is never put where a bot reads it, and the earlier copy goes
    let wrong = p.t.0.join("wrong.key");
    w(&wrong, &"7".repeat(64));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrong, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (c, v, e) = recent(&wrong);
    assert_eq!((c, v["pairs"][0]["digest"].is_null()), (0, true), "{v} {e}");
    assert!(!p.drive.join("from-hermes/digest").exists());
    // the skill a bot reads it with reads only its own pair's file
    let (c, v, e) = p.bp(&["mirror", "setup", "--print-skill"]);
    assert_eq!(c, 0, "{e}");
    let skill = v["skill"].as_str().unwrap();
    assert!(skill.starts_with("---\nname: mirror-recent\n"), "{skill}");
    assert!(skill.contains("Read only your own pair's file"), "{skill}");
}

#[test]
fn a_bot_read_from_the_box_gets_only_what_the_owner_approved() {
    let p = Pair::new("mirror-box-approved");
    let data = p.t.0.join("box/agent-data");
    let id = "0a1b2c3d-0000-4000-8000-00000000abce";
    w(
        &data.join(format!("agents/{id}/profile.json")),
        r#"{"name": "Demo Bot"}"#,
    );
    w(
        &data.join("workflows/demo-skill-0/SKILL.md"),
        "# Demo skill 0\\nExample steps.",
    );
    let digest = || {
        let out = p.drive.join("from-grok");
        p.bp(&[
            "mirror",
            "digest-items",
            "--pair",
            "demo-bot",
            "--agent-id",
            id,
            "--agent-data",
            s(&data),
            "--with-skills",
            "--out",
            s(&out),
        ])
    };
    let (c, _, e) = digest();
    assert_eq!(c, 0, "{e}");
    let r = init_with(&p, &[]);
    if r.0 == 2 && r.2.contains("a pair key was made") {
        let (c, _, e) = digest();
        assert_eq!(c, 0, "{e}");
    }
    // the init in these tests opts into `--approve runnable`
    let (c, v, e) = init_with(&p, &[]);
    assert_eq!(c, 0, "{v} {e}");
    p.sync();
    // a box read is partial: the only change that can be bound for the box is a new skill, and it waits for the owner
    w(
        &p.hermes.join("skills/mirror/demo-skill-new/SKILL.md"),
        "# New demo skill\nExample steps.\n",
    );
    let (_, v, e) = p.sync();
    let new = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["item"] == "skill:demo-skill-new")
        .cloned()
        .unwrap_or_else(|| panic!("{v} {e}"));
    assert_eq!(new["action"], "needs-approval", "{v}");
    assert_eq!(v["in_flight"].as_u64(), Some(0), "{v} {e}");
    // once the owner approves it, it is carried
    let (c, _, e) = p.bp(&["mirror", "approve", "skill:demo-skill-new", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (_, v, e) = p.sync();
    assert_eq!(v["in_flight"].as_u64(), Some(1), "{v} {e}");
}

#[test]
fn a_twin_that_cannot_chat_yet_is_said_loudly_and_doctor_names_the_fix() {
    let p = Pair::new("mirror-chat-ready");
    w(
        &p.export,
        &export_json("Example instructions.", "Example prompt.", 1, 0),
    );
    p.box_digest();
    p.init();
    // sync works without a gateway; chatting does not, and every status says so first
    let (_, v, e) = p.bp(&["mirror", "status"]);
    let n = v["chat_notice"]
        .as_str()
        .unwrap_or_else(|| panic!("{v} {e}"));
    assert!(
        n.starts_with(
            "Synced, but you can't chat with this bot in Hermes yet: no Hermes gateway record was found"
        ),
        "{n}"
    );
    let (c, v, _) = p.sync();
    assert!(v["chat_notice"].is_string(), "{c} {v}");
    let (c, v, e) = p.bp(&["mirror", "doctor"]);
    assert_eq!(c, 1, "{v} {e}");
    assert_eq!(v["checks"][0]["name"], "chat", "{v}");
    assert_eq!(v["checks"][0]["ok"], false, "{v}");
    assert!(
        v["checks"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("fix: hermes gateway start"),
        "{v}"
    );
    assert_eq!(v["checks"][1]["name"], "model", "{v}");
    assert_eq!(v["checks"][1]["ok"], false, "{v}");
    // a running gateway that serves it, and a model: nothing to say
    w(
        &p.hermes.join("gateway_state.json"),
        &format!(
            r#"{{"pid": {}, "gateway_state": "running", "served_profiles": ["default"]}}"#,
            std::process::id()
        ),
    );
    let cfg = std::fs::read_to_string(p.hermes.join("config.yaml")).unwrap_or_default();
    w(
        &p.hermes.join("config.yaml"),
        &format!("model: example-model\n{cfg}"),
    );
    let (c, v, e) = p.bp(&["mirror", "doctor"]);
    if v["checks"][0]["detail"]
        .as_str()
        .is_some_and(|d| d.contains("could not check"))
    {
        return; // no `kill` here to check a process with
    }
    assert_eq!(c, 0, "{v} {e}");
    let (_, v, e) = p.bp(&["mirror", "status"]);
    assert!(v["chat_notice"].is_null(), "{v} {e}");
}

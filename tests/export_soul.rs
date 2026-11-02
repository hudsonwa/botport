//! A grok-bot-export file as the source: `persona.instructions` goes into an owned SOUL.md section and
//! `user_facts` into USER.md. Fixtures are synthetic.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run(args: &[&str]) -> (i32, String) {
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

const EXPORT: &str = "tests/fixtures/grok-export/grok-bot-export.json";

fn cmd(c: &str, p: &Path, det: &Path, json: bool) -> (i32, String) {
    let (b, pd, d) = (
        root().join(EXPORT).to_string_lossy().to_string(),
        p.to_string_lossy().to_string(),
        det.to_string_lossy().to_string(),
    );
    let mut a = vec![c, "--bundle", &b, "--profile-dir", &pd, "--details", &d];
    if json {
        a.push("--json");
    }
    run(&a)
}

fn soul_items(out: &str) -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(out).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["area"] == "soul" || i["area"] == "user")
        .map(|i| {
            (
                format!(
                    "{}:{}",
                    i["area"].as_str().unwrap(),
                    i["name"].as_str().unwrap()
                ),
                i["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn export_instructions_reach_soul_and_user_facts_reach_user_md() {
    let t = std::env::temp_dir().join(format!("g2h-export-soul-{}", std::process::id()));
    let _ = fs::remove_dir_all(&t);
    let p = t.join("nova");
    fs::create_dir_all(&p).unwrap();
    fs::write(p.join("SOUL.md"), "# Hermes-side notes\nKeep me.\n").unwrap();
    let det = t.join("d.json");

    let (rc, out) = cmd("plan", &p, &det, true);
    assert_eq!(rc, 0, "{out}");
    let s = soul_items(&out);
    assert!(
        s.contains(&("soul:persona".into(), "create".into())),
        "{s:?}"
    );
    assert!(
        s.contains(&("user:USER.md".into(), "create".into())),
        "{s:?}"
    );

    let (rc, out) = cmd("apply", &p, &det, false);
    assert_eq!(rc, 0, "{out}");
    let soul = fs::read_to_string(p.join("SOUL.md")).unwrap();
    assert!(
        soul.starts_with("# Hermes-side notes\nKeep me.\n"),
        "{soul}"
    );
    assert!(soul.contains(
        "<!-- botport:begin persona -->\nYou are Nova, a research helper.\nCite every source.\n<!-- botport:end persona -->"
    ), "{soul}");
    let user = fs::read_to_string(p.join("memories/USER.md")).unwrap();
    assert!(
        user.contains("The user is called Robin Example")
            && user.contains("Time zone: Europe/Lisbon"),
        "{user}"
    );

    // re-apply is a no-op; drift is in sync
    let (rc, out) = cmd("apply", &p, &det, false);
    assert_eq!(rc, 0, "{out}");
    assert_eq!(fs::read_to_string(p.join("SOUL.md")).unwrap(), soul);
    assert_eq!(
        fs::read_to_string(p.join("memories/USER.md")).unwrap(),
        user
    );
    let (rc, out) = cmd("drift", &p, &det, false);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("IN SYNC"), "{out}");

    // a Hermes-side edit outside the section survives the next apply; drift sees an edited section
    fs::write(p.join("SOUL.md"), soul.replace("Keep me.", "Keep me too.")).unwrap();
    let (rc, out) = cmd("drift", &p, &det, false);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("IN SYNC"), "{out}");
    fs::write(
        p.join("SOUL.md"),
        soul.replace("Cite every source.", "Cite nothing."),
    )
    .unwrap();
    let (_, out) = cmd("drift", &p, &det, true);
    let s = soul_items(&out);
    assert!(
        s.iter()
            .any(|(n, st)| n == "soul:persona" && st != "in-sync"),
        "{s:?}"
    );
    fs::remove_dir_all(&t).unwrap();
}

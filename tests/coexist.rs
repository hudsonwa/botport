//! Coexistence with `hermes migrate grok-bot`: a grok-bot-export.json as the source, and a Hermes profile that
//! the upstream importer already filled (fixtures are synthetic look-alikes of its output).
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copy(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap().flatten() {
        let t = dst.join(e.file_name());
        if e.path().is_dir() {
            copy(&e.path(), &t);
        } else {
            fs::copy(e.path(), t).unwrap();
        }
    }
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

fn home(tag: &str) -> PathBuf {
    let t = std::env::temp_dir().join(format!("g2h-coexist-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&t);
    copy(
        &root().join("tests/fixtures/hermes-migrated/nova"),
        &t.join("nova"),
    );
    t
}

const EXPORT: &str = "tests/fixtures/grok-export/grok-bot-export.json";

#[test]
fn verify_accepts_a_grok_bot_export_file() {
    let (rc, out) = run(&["verify", "--bundle", &root().join(EXPORT).to_string_lossy()]);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("verify OK"), "{out}");
}

#[test]
fn plan_on_migrated_home_adopts_and_reports_duplicate() {
    let t = home("plan");
    let p = t.join("nova");
    let det = t.join("d.json");
    let (rc, out) = run(&[
        "plan",
        "--bundle",
        &root().join(EXPORT).to_string_lossy(),
        "--profile-dir",
        &p.to_string_lossy(),
        "--json",
        "--details",
        &det.to_string_lossy(),
    ]);
    assert_eq!(rc, 0, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let items = v["items"].as_array().unwrap();
    let find = |area: &str, name: &str| {
        items
            .iter()
            .filter(|i| i["area"] == area && i["name"] == name)
            .collect::<Vec<_>>()
    };
    // only the manual checklist is new; the job and the MCP server upstream made are adopted
    assert!(
        items
            .iter()
            .all(|i| i["status"] != "create" || i["name"] == "botport-manual.md"),
        "{out}"
    );
    assert_eq!(find("cron", "Nova: Sample report")[0]["status"], "in-sync");
    assert_eq!(find("manual", "connector Calendar")[0]["status"], "manual");
    let dd = find("skills", "grok-nova/daily-totals");
    assert_eq!(dd.len(), 1, "{out}");
    assert_eq!(dd[0]["status"], "in-sync");
    assert!(dd[0]["detail"]
        .as_str()
        .unwrap()
        .contains("grok-bot-imports/daily-totals"));
    let mem = find("skills", "grok-nova/grok-nova-memory");
    assert_eq!(mem[0]["status"], "in-sync", "{out}");
    let dup: Vec<&Value> = items
        .iter()
        .filter(|i| i["status"] == "duplicate")
        .collect();
    assert_eq!(dup.len(), 1, "{out}");
    let d = dup[0]["detail"].as_str().unwrap();
    assert!(
        d.contains("skills/grok-nova/cite-sources")
            && d.contains("skills/grok-bot-imports/cite-sources"),
        "{d}"
    );
    let cron = find("cron", "Nova: Sample report");
    assert_eq!(cron[0]["status"], "in-sync", "{out}");

    // apply writes no second copy and deletes neither
    let (rc, out) = run(&[
        "apply",
        "--bundle",
        &root().join(EXPORT).to_string_lossy(),
        "--profile-dir",
        &p.to_string_lossy(),
        "--details",
        &det.to_string_lossy(),
    ]);
    assert_eq!(rc, 0, "{out}");
    let names = |d: &Path| {
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with('.'))
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(&p.join("skills/grok-nova")), ["cite-sources"]);
    assert_eq!(
        names(&p.join("skills/grok-bot-imports")),
        ["cite-sources", "daily-totals"]
    );
    let mem = fs::read_to_string(p.join("memories/MEMORY.md")).unwrap();
    assert_eq!(mem.matches("wiki.example.test").count(), 1);
    let jobs = fs::read_to_string(p.join("cron/jobs.json")).unwrap();
    assert_eq!(jobs.matches("Sample report").count(), 1);
    fs::remove_dir_all(&t).unwrap();
}

/// A suspicious v1 memory line goes to the checklist (reason, no text); the apply still succeeds.
#[test]
fn suspicious_v1_memory_line_is_routed_not_fatal() {
    let t = home("mem");
    let p = t.join("nova");
    let src = fs::read_to_string(root().join(EXPORT)).unwrap();
    let ex = t.join("export.json");
    fs::write(
        &ex,
        src.replacen(
            "\"Weekly report goes out on Fridays\"",
            "\"Weekly report goes out on Fridays\", \"the vpn password: zqV1mem9x\"",
            1,
        ),
    )
    .unwrap();
    let det = t.join("d.json");
    let (rc, out) = run(&[
        "apply",
        "--bundle",
        &ex.to_string_lossy(),
        "--profile-dir",
        &p.to_string_lossy(),
        "--details",
        &det.to_string_lossy(),
    ]);
    assert_eq!(rc, 0, "{out}");
    assert!(!out.contains("zqV1mem9x"), "{out}");
    let manual = fs::read_to_string(p.join("botport-manual.md")).unwrap();
    assert!(manual.contains("memory"), "{manual}");
    let mut stack = vec![p.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                let b = fs::read(e.path()).unwrap();
                assert!(
                    !String::from_utf8_lossy(&b).contains("zqV1mem9"),
                    "{}",
                    e.path().display()
                );
            }
        }
    }
    fs::remove_dir_all(&t).unwrap();
}

/// A suspicious v1 skill body routes that skill to the checklist (name and reason, no text); the apply still succeeds.
#[test]
fn suspicious_v1_skill_body_is_routed_not_fatal() {
    let t = home("skill");
    let p = t.join("nova");
    let src = fs::read_to_string(root().join(EXPORT)).unwrap();
    let ex = t.join("export.json");
    fs::write(
        &ex,
        src.replacen(
            "Collect data.",
            "Collect data. api_key=Zq7Wv3Xk9Lm2Np8Rt4Yb",
            1,
        ),
    )
    .unwrap();
    let det = t.join("d.json");
    let (rc, out) = run(&[
        "apply",
        "--bundle",
        &ex.to_string_lossy(),
        "--profile-dir",
        &p.to_string_lossy(),
        "--details",
        &det.to_string_lossy(),
    ]);
    assert_eq!(rc, 0, "{out}");
    let manual = fs::read_to_string(p.join("botport-manual.md")).unwrap();
    assert!(
        manual.contains("skill daily-totals") && manual.contains("looks like it holds a secret"),
        "{manual}"
    );
    let mut stack = vec![t.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else if e.path() != ex {
                let b = fs::read(e.path()).unwrap();
                assert!(
                    !String::from_utf8_lossy(&b).contains("Zq7Wv3Xk9"),
                    "{}",
                    e.path().display()
                );
            }
        }
    }
    fs::remove_dir_all(&t).unwrap();
}

/// A Hermes home and a details dir with a random-looking segment (`mktemp -d`, outside the system temp dir) are
/// the user's own paths: plan and apply print the full report.
#[test]
fn random_looking_home_and_out_paths_are_not_secrets() {
    let t = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("tmp.286U0Niu1x-{}", std::process::id()));
    let _ = fs::remove_dir_all(&t);
    let hh = t.join("hermes");
    copy(
        &root().join("tests/fixtures/hermes-migrated/nova"),
        &hh.join("profiles/nova"),
    );
    let out = t.join("out.7Gq2Xk9PzW");
    for cmd in ["plan", "apply"] {
        let det = out.join(format!("{cmd}.json"));
        let (rc, o) = run(&[
            cmd,
            "--bundle",
            &root().join(EXPORT).to_string_lossy(),
            "--hermes-home",
            &hh.to_string_lossy(),
            "--profile",
            "nova",
            "--no-cron",
            "--details",
            &det.to_string_lossy(),
            "--json",
        ]);
        assert_eq!(rc, 0, "{cmd}: {o}");
        let v: serde_json::Value = serde_json::from_str(&o).unwrap_or_else(|e| panic!("{e}: {o}"));
        assert!(v["dest"].as_str().unwrap().contains("286U0Niu1x"), "{v}");
        if cmd == "apply" {
            assert!(v["restore_point"].as_str().is_some(), "{v}");
        }
    }
    fs::remove_dir_all(&t).unwrap();
}

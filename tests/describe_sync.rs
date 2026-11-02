//! The bot's description (schedules, connectors, credentials) through plan/apply on a throwaway Hermes home with a
//! fake `hermes` CLI (no network, no real Hermes).
mod common;
use common::{s, w, Tmp};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DESCRIPTION: &str = "# Not captured by the export\n\n## The bot's description\n\n\
### Schedules\n\
- **Sample report**: weekdays 08:00 America/Chicago — prompt: \"Summarise the sample log\"\n\
- **Ping check**: hourly — prompt: \"Check the pings\"\n\
- **Nightly export**: runs on another machine (the main server), not on Grok\n\n\
### Connectors\n\
- **knowledge**: MCP server at https://demo.example.test/mcp (tools: find_items, health)\n\
- **Web search**: built-in\n\n\
### Credentials\n\
- `DEMO_TOKEN` — in the profile's .env\n\
- STRIPE_KEY in vault, value is pa55w0rdXYZ\n";

/// A legacy hand export with a description, a throwaway home, and a fake hermes that records `cron create`.
fn setup(t: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let g = t.join("grok");
    w(&g.join("profile.json"), r#"{"name":"Nova"}"#);
    w(&g.join("MEMORY.md"), "# Nova memory\n- likes tea\n");
    w(
        &g.join("skills/alpha/SKILL.md"),
        "---\nname: alpha\n---\nbody\n",
    );
    w(&g.join("NOT_CAPTURED.md"), DESCRIPTION);
    let hh = t.join("hermes");
    fs::create_dir_all(hh.join("profiles/nova")).unwrap();
    let fake = t.join("fake-hermes");
    w(
        &fake,
        r#"#!/bin/sh
# fake hermes: -p PROFILE cron create --name N --deliver D [--paused] [--paused-reason R] SCHEDULE PROMPT
prof=$2; shift 3; sub=$1; shift
d="$HERMES_HOME/profiles/$prof/cron"; mkdir -p "$d"
echo "$sub $*" >> "$d/calls.log"
[ "$sub" = create ] || exit 0
enabled=true
while [ $# -gt 2 ]; do
  case "$1" in
    --name) name=$2; shift 2 ;;
    --deliver) deliver=$2; shift 2 ;;
    --paused) enabled=false; shift ;;
    --paused-reason) shift 2 ;;
    *) shift ;;
  esac
done
printf '{"jobs":[{"id":"j1","name":"%s","prompt":"%s","deliver":"%s","schedule":{"kind":"cron","expr":"%s"},"enabled":%s}]}\n' \
  "$name" "$2" "$deliver" "$1" "$enabled" > "$d/jobs.json"
"#,
    );
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    (g, hh, fake)
}

fn run(cmd: &str, g: &Path, hh: &Path, fake: &Path, extra: &[&str]) -> (i32, Value) {
    let mut a = vec![
        cmd,
        "--bundle",
        s(g),
        "--hermes-home",
        s(hh),
        "--profile",
        "nova",
        "--hermes-bin",
        s(fake),
        "--json",
    ];
    a.extend(extra);
    let o = Command::new(env!("CARGO_BIN_EXE_grok2hermes"))
        .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR")) // not ~/.local/state
        .args(&a)
        .env("HERMES_HOME", "/nonexistent-must-not-be-used")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    let v: Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("{e}: {out} {}", String::from_utf8_lossy(&o.stderr)));
    (o.status.code().unwrap_or(-1), v)
}

fn status(v: &Value, area: &str, name: &str) -> String {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["area"] == area && i["name"] == name)
        .map(|i| i["status"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("no item {area} {name}: {v}"))
}

#[test]
fn describe_items_plan_apply_and_stay_in_sync() {
    let t = Tmp::new("main");
    let (g, hh, fake) = setup(&t.0);
    let prof = hh.join("profiles/nova");
    let (rc, p) = run("plan", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0, "{p}");
    assert_eq!(status(&p, "cron", "Nova: Sample report"), "create");
    assert_eq!(status(&p, "mcp", "mcp_servers.knowledge"), "create");
    assert_eq!(status(&p, "manual", "botport-manual.md"), "create");
    for n in [
        "schedule Ping check",
        "schedule Nightly export",
        "connector Web search",
        "credential DEMO_TOKEN",
        "credential STRIPE_KEY",
    ] {
        assert_eq!(status(&p, "manual", n), "manual", "{n}");
    }
    let text = p.to_string();
    assert!(!text.contains("pa55w0rdXYZ"), "{text}");
    // English Hermes' parse_schedule rejects is a manual item with its text, never a guessed cron line
    assert!(
        text.contains("not imported: 'hourly' is not a schedule Hermes accepts; set it by hand"),
        "{text}"
    );
    // deterministic
    assert_eq!(run("plan", &g, &hh, &fake, &[]).1, p);

    let (rc, a) = run("apply", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0, "{a}");
    let calls = fs::read_to_string(prof.join("cron/calls.log")).unwrap();
    assert!(
        calls.contains("--name Nova: Sample report") && calls.contains("--paused"),
        "{calls}"
    );
    assert!(calls.contains("0 8 * * 1-5"), "{calls}");
    assert!(
        calls.contains("America/Chicago"),
        "timezone explicit: {calls}"
    );
    let cfg = fs::read_to_string(prof.join("config.yaml")).unwrap();
    assert!(
        cfg.contains("mcp_servers") && cfg.contains("https://demo.example.test/mcp"),
        "{cfg}"
    );
    let manual = fs::read_to_string(prof.join("botport-manual.md")).unwrap();
    assert!(
        manual.contains("- [ ] ") && manual.contains("DEMO_TOKEN"),
        "{manual}"
    );
    assert!(!manual.contains("pa55w0rdXYZ"), "{manual}");
    let rps: Vec<_> = fs::read_dir(prof.join("backups/botport"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("restore-"))
        .collect();
    assert_eq!(rps.len(), 1);

    // second plan: nothing to do but the manual list
    let (rc, p2) = run("plan", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0);
    for i in p2["items"].as_array().unwrap() {
        assert!(
            i["status"] == "in-sync" || i["status"] == "manual",
            "not settled: {i}"
        );
    }
    // the job came from a Grok routine: never requested back from the Grok side
    let (_, h) = run("drift", &g, &hh, &fake, &["--direction", "h2g"]);
    assert_eq!(h["h2g"]["cron"], 0, "{h}");
    // a ticked box is the user's, not a change
    w(
        &prof.join("botport-manual.md"),
        &manual.replacen("- [ ] ", "- [x] ", 1),
    );
    assert_eq!(
        status(
            &run("plan", &g, &hh, &fake, &[]).1,
            "manual",
            "botport-manual.md"
        ),
        "in-sync"
    );

    // Hermes-side edits are conflicts, never overwritten
    w(
        &prof.join("config.yaml"),
        &cfg.replace(
            "https://demo.example.test/mcp",
            "https://mine.example.test/mcp",
        ),
    );
    let jobs = fs::read_to_string(prof.join("cron/jobs.json")).unwrap();
    w(
        &prof.join("cron/jobs.json"),
        &jobs.replace("Summarise the sample log", "My own prompt"),
    );
    let (_, p3) = run("plan", &g, &hh, &fake, &[]);
    assert_eq!(status(&p3, "mcp", "mcp_servers.knowledge"), "conflict");
    assert_eq!(status(&p3, "cron", "Nova: Sample report"), "conflict");
    run("apply", &g, &hh, &fake, &[]);
    assert!(fs::read_to_string(prof.join("config.yaml"))
        .unwrap()
        .contains("mine.example.test"));
    assert!(fs::read_to_string(prof.join("cron/jobs.json"))
        .unwrap()
        .contains("My own prompt"));
}

#[test]
fn existing_upstream_job_and_equal_mcp_entry_are_adopted() {
    let t = Tmp::new("adopt");
    let (g, hh, fake) = setup(&t.0);
    let prof = hh.join("profiles/nova");
    w(
        &prof.join("cron/jobs.json"),
        r#"{"jobs":[{"id":"u1","name":"Nova: Sample report","prompt":"Upstream wording","schedule":{"kind":"cron","expr":"0 8 * * 1-5"},"enabled":false}]}"#,
    );
    w(
        &prof.join("config.yaml"),
        "mcp_servers:\n  knowledge:\n    url: https://demo.example.test/mcp\n    tools:\n      include: [find_items, health]\n",
    );
    let (_, p) = run("plan", &g, &hh, &fake, &[]);
    assert_eq!(status(&p, "cron", "Nova: Sample report"), "in-sync");
    assert_eq!(status(&p, "mcp", "mcp_servers.knowledge"), "in-sync");
    let (rc, a) = run("apply", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0, "{a}");
    assert!(
        !prof.join("cron/calls.log").exists(),
        "no second job is created"
    );
}

#[test]
fn connector_the_config_template_already_serves_is_not_doubled() {
    let t = Tmp::new("tpl");
    let (g, hh, fake) = setup(&t.0);
    w(
        &t.0.join("tpl/config.yaml"),
        "mcp_servers:\n  demo:\n    url: https://demo.example.test/mcp\n",
    );
    let spec = t.0.join("spec.toml");
    w(
        &spec,
        &format!(
            "profile = \"nova\"\nbase = \"{}\"\n[config]\ntemplate = \"config.yaml\"\n",
            t.0.join("tpl").display()
        ),
    );
    let (_, p) = run("plan", &g, &hh, &fake, &["--spec", s(&spec)]);
    let mcp: Vec<&Value> = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["area"] == "mcp")
        .collect();
    assert!(mcp.is_empty(), "{mcp:?}");
}

/// Every fake secret of the corpus (seeded-secrets.txt: known evasions found in review plus our own), seeded into every field
/// position, never reaches describe.json, plan/apply --json, the manual file, config.yaml or the hermes cron calls.
#[test]
fn a_seeded_secret_reaches_no_output() {
    let secrets = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/describe/seeded-secrets.txt"
    ))
    .unwrap();
    let d = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/describe/seeded.md"
    ))
    .unwrap();
    let t = Tmp::new("seed");
    let (g, hh, fake) = setup(&t.0);
    w(&g.join("NOT_CAPTURED.md"), &d);
    let dj = t.0.join("describe.json");
    let o = Command::new(env!("CARGO_BIN_EXE_grok2hermes"))
        .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR")) // not ~/.local/state
        .args([
            "describe",
            "--describe",
            s(&g.join("NOT_CAPTURED.md")),
            "--out",
            s(&dj),
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let check = |what: &str, text: &str| {
        for x in secrets.lines().filter(|l| !l.is_empty()) {
            assert!(!text.contains(x), "secret {x} in {what}: {text}");
        }
    };
    check("describe.json", &fs::read_to_string(&dj).unwrap());
    check("describe stdout", &String::from_utf8_lossy(&o.stdout));
    let (_, p) = run("plan", &g, &hh, &fake, &[]);
    check("plan --json", &p.to_string());
    let (rc, a) = run("apply", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0, "{a}");
    check("apply --json", &a.to_string());
    let prof = hh.join("profiles/nova");
    check(
        "manual",
        &fs::read_to_string(prof.join("botport-manual.md")).unwrap(),
    );
    if let Ok(c) = fs::read_to_string(prof.join("config.yaml")) {
        check("config.yaml", &c);
    }
    // every seeded schedule is suspicious: no cron job is created at all
    let calls = fs::read_to_string(prof.join("cron/calls.log")).unwrap_or_default();
    check("hermes cron calls", &calls);
    // A suspicious schedule is not written at all: it is on the checklist, with the reason.
    assert!(!calls.contains("Nova: Login"), "{calls}");
    assert_eq!(status(&p, "manual", "schedule Login"), "manual");
    let f = "cron/jobs.json";
    if let Ok(c) = fs::read_to_string(prof.join(f)) {
        check(f, &c);
    }
    let dj = fs::read_to_string(&dj).unwrap();
    assert!(
        !dj.contains("\"raw\""),
        "describe.json keeps no raw text: {dj}"
    );
    assert!(
        dj.contains("\"reason\""),
        "a reason for each routed item: {dj}"
    );
}

/// One suspicious and one clean schedule: the suspicious one reaches no written file and is on the checklist with its
/// reason; the clean one is written byte-identical, paused, as "<Bot>: <routine>".
#[test]
fn a_suspicious_schedule_is_routed_to_the_checklist_and_a_clean_one_written_unchanged() {
    let t = Tmp::new("route");
    let (g, hh, fake) = setup(&t.0);
    let clean = "Summarise the key points; the key to success";
    w(
        &g.join("NOT_CAPTURED.md"),
        &format!(
            "# Not captured\n\n## The bot's description\n\n### Schedules\n\
- **Totals**: daily at 9am UTC — prompt: \"{clean}\"\n\
- **Weather**: daily at 7am UTC — prompt: \"send me the weather at 7\"\n\
- **PRs**: daily at 8am UTC — prompt: \"review open PRs\"\n\
- **Budget**: daily at 11am UTC — prompt: \"keep the token budget small\"\n\
- **Login**: daily at 10am UTC — prompt: \"Log in with password QVXrouteval7 and check\"\n\n\
### Connectors\n\
- **crm**: MCP at https://crm.example.test/mcp?api_token=QVXrouteurl8\n"
        ),
    );
    let dj = t.0.join("describe.json");
    let o = Command::new(env!("CARGO_BIN_EXE_grok2hermes"))
        .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR")) // not ~/.local/state
        .args([
            "describe",
            "--describe",
            s(&g.join("NOT_CAPTURED.md")),
            "--out",
            s(&dj),
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (_, p) = run("plan", &g, &hh, &fake, &[]);
    assert_eq!(status(&p, "cron", "Nova: Totals"), "create");
    assert_eq!(status(&p, "cron", "Nova: Weather"), "create");
    assert_eq!(status(&p, "cron", "Nova: PRs"), "create");
    assert_eq!(status(&p, "manual", "schedule Login"), "manual");
    assert_eq!(status(&p, "manual", "schedule Budget"), "manual");
    assert_eq!(status(&p, "manual", "connector crm"), "manual");
    let (rc, a) = run("apply", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0, "{a}");
    let prof = hh.join("profiles/nova");
    let calls = fs::read_to_string(prof.join("cron/calls.log")).unwrap();
    assert!(
        calls.contains(&format!("0 9 * * * {clean}")),
        "byte-identical prompt: {calls}"
    );
    for (n, pr) in [
        ("Weather", "send me the weather at 7"),
        ("PRs", "review open PRs"),
    ] {
        assert!(
            calls.contains(pr) && calls.contains(&format!("--name Nova: {n}")),
            "{calls}"
        );
    }
    assert!(
        calls.contains("--name Nova: Totals") && calls.contains("--paused"),
        "{calls}"
    );
    assert!(
        !calls.contains("Login") && !calls.contains("token budget"),
        "{calls}"
    );
    let jobs = fs::read_to_string(prof.join("cron/jobs.json")).unwrap();
    assert!(jobs.contains("\"enabled\":false"), "{jobs}");
    let manual = fs::read_to_string(prof.join("botport-manual.md")).unwrap();
    assert!(
        manual.contains("schedule Login") && manual.contains("looks like it holds a secret ("),
        "reason: {manual}"
    );
    assert!(manual.contains("connector crm"), "{manual}");
    assert!(
        manual.contains("schedule Budget") && manual.contains("secret keyword ('token')"),
        "keyword reason: {manual}"
    );
    let mut written = vec![
        fs::read_to_string(&dj).unwrap(),
        p.to_string(),
        a.to_string(),
        calls,
        jobs,
        manual,
    ];
    if let Ok(c) = fs::read_to_string(prof.join("config.yaml")) {
        assert!(!c.contains("crm.example.test"), "{c}");
        written.push(c);
    }
    for x in written {
        for v in ["QVXrouteval7", "QVXrouteurl8"] {
            assert!(!x.contains(v), "{v} in {x}");
        }
    }
}

/// Windows of a seeded secret that must never be written: every 6+ char substring that is not a plain lowercase
/// word (`passwo` in "QVXhunterpasswordtwo" is the keyword the checklist reason names, not the value).
fn windows(x: &str) -> Vec<String> {
    let c: Vec<char> = x.chars().collect();
    if c.len() < 6 {
        return vec![x.to_string()];
    }
    (0..=c.len() - 6)
        .map(|i| c[i..i + 6].iter().collect::<String>())
        .filter(|w| !w.chars().all(|ch| ch.is_ascii_lowercase()))
        .collect()
}

/// Every file under `root` (JSON/YAML/MD/text alike) as text, by path.
fn walk(root: &Path, out: &mut Vec<(PathBuf, String)>) {
    for e in fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push((
                p.clone(),
                String::from_utf8_lossy(&fs::read(&p).unwrap()).into(),
            ));
        }
    }
}

/// The supervisor gate: each seeded corpus (seeded.md, and a generated matrix of every secret in every field
/// position) through export -> describe -> plan -> apply -> export-hermes on a throwaway home; no file written
/// anywhere (nor any --json) holds a seeded secret or a 6-char window of one. Run twice: as shipped, and with the
/// per-field scan switched off (G2H_TEST_SKIP_FIELD_SCAN, debug builds only), where the write-boundary guard and
/// the hermes argv guard alone must keep every secret off disk (a refused write is fine, a written secret is not).
#[test]
fn gate_no_seeded_secret_survives_in_any_written_file() {
    let secrets: Vec<String> = include_str!("fixtures/describe/seeded-secrets.txt")
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let mut corpora = vec![include_str!("fixtures/describe/seeded.md").to_string()];
    let mut m = String::from("# Not captured\n\n## The bot's description\n\n");
    // every secret in a rule-(B) shape in every field (key=value, key: value, url credentials); a keyword-only
    // mention is rule (A)'s job and a documented limit of the guard
    for (i, x) in secrets.iter().enumerate() {
        let vs = [
            format!("password: {x}"),
            format!("api_key={x}"),
            format!("token: {x}"),
        ];
        for (j, v) in vs.iter().enumerate() {
            m += &format!(
                "### Instructions\nBe nice. {v}\n\n### Schedules\n\
- **S{i}x{j}**: daily at 9am UTC — prompt: \"use {v}\"\n\
- **{v}**: daily at 9am UTC — prompt: \"hi\"\n\
- **R{i}x{j}**: every {v} — prompt: \"hi\"\n\n\
### Connectors\n\
- **c{i}x{j}**: MCP server at https://c{i}.example.test/mcp?token={x}\n\
- **d{i}x{j}**: MCP server at https://u:{x}@d{i}.example.test/mcp\n\
- **e{i}x{j}**: MCP server at https://e{i}.example.test/mcp (tools: {v})\n\
- **{v}**: MCP server at https://f{i}.example.test/mcp\n\n\
### Credentials\n\
- `K{i}x{j}` — {v}\n- K{i}x{j}B in {v}\n- {v}\n\n"
            );
        }
    }
    corpora.push(m);
    // schedules only, so the per-field-scan-off run reaches `hermes cron create` (the argv guard)
    let mut m = String::from("# Not captured\n\n## The bot's description\n\n### Schedules\n");
    for (i, x) in secrets.iter().enumerate() {
        m += &format!("- **T{i}**: daily at 9am UTC — prompt: \"use token={x}\"\n");
    }
    corpora.push(m);
    for skip in [false, true] {
        for (ni, d) in corpora.iter().enumerate() {
            let n = format!("{ni}{}", if skip { " (field scan off)" } else { "" });
            let t = Tmp::new(&format!("gate{ni}{}", skip as u8));
            let (src, hh, fake) = setup(&t.0);
            w(&src.join("NOT_CAPTURED.md"), d);
            let desc = t.0.join("in-describe.md");
            w(&desc, d);
            let out = t.0.join("out");
            let g = out.join("bundle");
            let bin = env!("CARGO_BIN_EXE_grok2hermes");
            let mut stdout = String::new();
            for args in [
                vec![
                    "export",
                    "--out",
                    s(&g),
                    "--from-dir",
                    s(&src),
                    "--describe",
                    s(&desc),
                ],
                vec![
                    "describe",
                    "--bundle",
                    s(&g),
                    "--out",
                    s(&out.join("describe.json")),
                    "--json",
                ],
            ] {
                let o = Command::new(bin)
                    .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR"))
                    .args(&args)
                    .env("G2H_TEST_SKIP_FIELD_SCAN", if skip { "1" } else { "" })
                    .args([
                        "--details",
                        s(&out.join(format!("{}-details.json", args[0]))),
                    ])
                    .output()
                    .unwrap();
                stdout += &String::from_utf8_lossy(&o.stdout);
                stdout += &String::from_utf8_lossy(&o.stderr);
            }
            let (hh_, fk) = (s(&hh).to_string(), s(&fake).to_string());
            let hx = out.join("hexport");
            let common = [
                "--bundle",
                s(&g),
                "--hermes-home",
                &hh_,
                "--profile",
                "nova",
                "--json",
            ];
            for cmd in ["plan", "apply", "export-hermes"] {
                let mut a = vec![cmd];
                a.extend(common);
                if cmd == "export-hermes" {
                    a.extend(["--out", s(&hx)]);
                } else {
                    a.extend(["--hermes-bin", &fk]);
                }
                let det = out.join(format!("{cmd}-details.json"));
                let o = Command::new(bin)
                    .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR"))
                    .args(&a)
                    .args(["--details", s(&det)])
                    .env("HERMES_HOME", "/nonexistent-must-not-be-used")
                    .env("G2H_TEST_SKIP_FIELD_SCAN", if skip { "1" } else { "" })
                    .output()
                    .unwrap();
                if cmd != "export-hermes" && !skip {
                    assert!(
                        o.status.success(),
                        "corpus {n}: {cmd} failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                }
                stdout += &String::from_utf8_lossy(&o.stdout);
                stdout += &String::from_utf8_lossy(&o.stderr);
            }
            // apply straight from the hand export (nothing of ours written before it): reaches `hermes cron create`
            let hh2 = t.0.join("hermes2");
            fs::create_dir_all(hh2.join("profiles/nova")).unwrap();
            let o = Command::new(bin)
                .env("XDG_STATE_HOME", env!("CARGO_TARGET_TMPDIR"))
                .args(["apply", "--bundle", s(&src), "--hermes-home", s(&hh2)])
                .args(["--profile", "nova", "--json", "--hermes-bin", &fk])
                .args(["--details", s(&out.join("apply2-details.json"))])
                .env("HERMES_HOME", "/nonexistent-must-not-be-used")
                .env("G2H_TEST_SKIP_FIELD_SCAN", if skip { "1" } else { "" })
                .output()
                .unwrap();
            assert!(
                skip || o.status.success(),
                "corpus {n}: apply from the hand export failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            stdout += &String::from_utf8_lossy(&o.stdout);
            stdout += &String::from_utf8_lossy(&o.stderr);
            let mut files = vec![(t.0.join("<stdout/stderr>"), stdout)];
            walk(&out, &mut files);
            walk(&hh, &mut files);
            walk(&hh2, &mut files);
            assert!(
                skip || hh.join("profiles/nova/botport-manual.md").is_file(),
                "corpus {n}: the pipeline ran through apply"
            );
            for (p, text) in &files {
                for x in &secrets {
                    for win in windows(x) {
                        assert!(
                            !text.contains(&win),
                            "corpus {n}: window {win} of {x} in {}: {}",
                            p.display(),
                            text.lines()
                                .filter(|l| l.contains(&win))
                                .collect::<Vec<_>>()
                                .join(" / ")
                        );
                    }
                }
            }
        }
    }
}

/// The manual checklist file is advice, not synced state: drift stays IN SYNC without it (it is on the `manual:` line).
#[test]
fn drift_does_not_count_a_missing_checklist_file() {
    let t = Tmp::new("drift-manual");
    let (g, hh, fake) = setup(&t.0);
    let (rc, a) = run("apply", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0, "{a}");
    fs::remove_file(hh.join("profiles/nova/botport-manual.md")).unwrap();
    let (rc, d) = run("drift", &g, &hh, &fake, &[]);
    assert_eq!(rc, 0, "{d}");
    assert_eq!(d["in_sync"], true, "{d}");
    // plan still offers to create it
    let (_, p) = run("plan", &g, &hh, &fake, &[]);
    assert_eq!(status(&p, "manual", "botport-manual.md"), "create");
}

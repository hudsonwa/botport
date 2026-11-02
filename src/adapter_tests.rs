//! Adapter-trait tests: platform read -> BotModel -> stable JSON, and write -> read round trips.
use crate::adapters::{grok_bot::GrokBot, hermes::HermesProfile};
use crate::core::adapter::Adapter;
use crate::core::model::BotModel;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("g2h-adapter-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn grok_bot_reads_acme_to_stable_json() {
    let m = GrokBot.read(&root().join("examples/acme/grok")).unwrap();
    let golden = root().join("tests/fixtures/model/acme-grok.json");
    if std::env::var_os("G2H_BLESS").is_some() {
        std::fs::write(&golden, m.to_json()).unwrap();
    }
    let want = std::fs::read_to_string(golden).unwrap();
    assert_eq!(m.to_json(), want);
    assert_eq!(BotModel::from_json(&want).unwrap(), m);
}

#[test]
fn grok_bot_write_then_read_round_trips() {
    let m = GrokBot.read(&root().join("examples/acme/grok")).unwrap();
    let d = scratch("grok");
    GrokBot.write(&m, &d).unwrap();
    assert_eq!(GrokBot.read(&d).unwrap(), m);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn hermes_write_then_read_round_trips() {
    let mut m = GrokBot.read(&root().join("examples/acme/grok")).unwrap();
    m.extras.clear();
    let d = scratch("hermes");
    HermesProfile.write(&m, &d).unwrap();
    let back = HermesProfile.read(&d).unwrap();
    assert_eq!(back.persona.instructions, m.persona.instructions);
    let texts = |x: &BotModel| x.memory.iter().map(|f| f.text.clone()).collect::<Vec<_>>();
    assert_eq!(texts(&back), texts(&m));
    assert_eq!(back.skills, m.skills);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn core_has_no_platform_names() {
    fn walk(d: &std::path::Path, n: &mut usize) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n);
                continue;
            }
            let t = std::fs::read_to_string(&p).unwrap().to_lowercase();
            for bad in ["grok", "hermes"] {
                assert!(!t.contains(bad), "{} mentions {bad}", p.display());
            }
            *n += 1;
        }
    }
    let mut n = 0;
    walk(&root().join("src/core"), &mut n);
    assert!(n >= 3);
}

#[test]
fn grok_bot_reads_export_json_to_golden() {
    let src = root().join("tests/fixtures/grok-export/grok-bot-export.json");
    let m = GrokBot.read(&src).unwrap();
    let golden = root().join("tests/fixtures/model/grok-export.json");
    if std::env::var_os("G2H_BLESS").is_some() {
        std::fs::write(&golden, m.to_json()).unwrap();
    }
    assert_eq!(m.to_json(), std::fs::read_to_string(golden).unwrap());
    // escaped newlines unescaped; long app name -> short stable id; truncated memories kept and counted
    let digest = &m
        .skills
        .iter()
        .find(|s| s.name == "daily-totals")
        .unwrap()
        .files["SKILL.md"];
    assert!(digest.contains("Collect data.\nSummarise") && !digest.contains("\\n"));
    assert_eq!(m.connectors[0].name, "notes-service-mcp");
    assert_eq!(m.extras["grok-bot"]["truncated_memories"], 1);
    assert!(m
        .memory
        .iter()
        .any(|f| f.text.ends_with("[truncated in memory recall]")));
}

#[test]
fn grok_bot_write_keeps_binary_skill_files() {
    let d = scratch("bin-src");
    std::fs::write(d.join("profile.json"), "{\"name\":\"B\"}").unwrap();
    std::fs::create_dir_all(d.join("skills/s")).unwrap();
    std::fs::write(d.join("skills/s/SKILL.md"), "x\n").unwrap();
    std::fs::write(d.join("skills/s/logo.png"), [0xff_u8, 0xfe, 0x00, 0x80]).unwrap();
    let m = GrokBot.read(&d).unwrap();
    let o = scratch("bin-out");
    GrokBot.write(&m, &o).unwrap();
    assert_eq!(
        std::fs::read(o.join("skills/s/logo.png")).unwrap(),
        [0xff_u8, 0xfe, 0x00, 0x80]
    );
    std::fs::remove_dir_all(&d).unwrap();
    std::fs::remove_dir_all(&o).unwrap();
}

#[test]
fn hermes_write_keeps_user_and_memory_tiers() {
    let m = GrokBot
        .read(&root().join("tests/fixtures/grok-export/grok-bot-export.json"))
        .unwrap();
    let d = scratch("tiers");
    HermesProfile.write(&m, &d).unwrap();
    let user = std::fs::read_to_string(d.join("memories/USER.md")).unwrap();
    let mem = std::fs::read_to_string(d.join("memories/MEMORY.md")).unwrap();
    assert!(user.contains("Robin Example") && !mem.contains("Robin Example"));
    assert!(mem.contains("wiki.example.test"));
    let back = HermesProfile.read(&d).unwrap();
    HermesProfile.write(&back, &d).unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join("memories/USER.md")).unwrap(),
        user
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn hermes_read_never_reads_env_or_auth_and_scrubs_secret_config_keys() {
    use std::os::unix::fs::PermissionsExt;
    let d = scratch("scrub");
    std::fs::write(d.join(".env"), "DEMO_TOKEN=envmarker1234\n").unwrap();
    std::fs::write(d.join("auth.json"), r#"{"t":"authmarker1234"}"#).unwrap();
    // unreadable: a read of either would fail the whole read
    for f in [".env", "auth.json"] {
        std::fs::set_permissions(d.join(f), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    std::fs::write(
        d.join("config.yaml"),
        "model:\n  default: example-model\nmcp_servers:\n  demo:\n    url: https://demo.example.test/mcp\n    headers:\n      X-Api: hdrmarker1234\n    auth: authcfgmarker\n    bearer: bearermarker\n    credential: credmarker\n    Authorization: azmarker\n",
    )
    .unwrap();
    let m = HermesProfile.read(&d).unwrap();
    let j = m.to_json();
    for leak in [
        "envmarker1234",
        "authmarker1234",
        "hdrmarker1234",
        "authcfgmarker",
        "bearermarker",
        "credmarker",
        "azmarker",
    ] {
        assert!(!j.contains(leak), "{leak} leaked: {j}");
    }
    assert!(j.contains("https://demo.example.test/mcp"), "{j}");
    for f in [".env", "auth.json"] {
        std::fs::set_permissions(d.join(f), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn hermes_write_carries_schedules_connectors_and_credential_names() {
    let m = GrokBot.read(&root().join("examples/acme/grok")).unwrap();
    assert!(!m.schedules.is_empty() && !m.connectors.is_empty() && !m.credentials.is_empty());
    let d = scratch("full");
    HermesProfile.write(&m, &d).unwrap();
    let jobs: serde_json::Value =
        serde_json::from_slice(&std::fs::read(d.join("cron/jobs.json")).unwrap()).unwrap();
    let jobs = jobs["jobs"].as_array().unwrap();
    assert_eq!(
        jobs.len(),
        m.schedules.iter().filter(|s| s.cron.is_some()).count()
    );
    assert!(jobs.iter().all(|j| j["enabled"] == false), "{jobs:?}");
    let cfg = std::fs::read_to_string(d.join("config.yaml")).unwrap();
    assert!(cfg.contains("https://visitors.example.test/mcp"), "{cfg}");
    let manual = std::fs::read_to_string(d.join("botport-manual.md")).unwrap();
    assert!(
        manual.contains("credential VISITOR_LOG_TOKEN") && manual.contains("connector Web search")
    );
    let back = HermesProfile.read(&d).unwrap();
    let names = |x: &BotModel| {
        x.credentials
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&back), names(&m));
    std::fs::remove_dir_all(&d).unwrap();
}

/// fixture -> read -> write -> read -> write -> read: returns the models of every read.
fn hermes_rounds(tag: &str, src: &std::path::Path, n: usize) -> Vec<BotModel> {
    let mut out = vec![HermesProfile.read(src).unwrap()];
    for i in 0..n {
        let d = scratch(&format!("{tag}-{i}"));
        HermesProfile.write(out.last().unwrap(), &d).unwrap();
        out.push(HermesProfile.read(&d).unwrap());
        std::fs::remove_dir_all(&d).unwrap();
    }
    out
}

#[test]
fn hermes_round_trip_keeps_cron_names_and_config_in_model_fields() {
    // issue #8: cron jobs and MCP servers come back as schedules and connectors, the same on every round trip
    let rounds = hermes_rounds(
        "rounds-fixture",
        &root().join("tests/fixtures/conformance/hermes"),
        3,
    );
    let first = &rounds[0];
    assert_eq!(first.schedules[0].name, "Conformance: Nightly tidy");
    assert_eq!(first.connectors[0].name, "visitor-log");
    for back in &rounds[1..] {
        assert_eq!(back.schedules, first.schedules);
        assert_eq!(back.connectors, first.connectors);
        assert_eq!(back.credentials, first.credentials);
        assert_eq!(back.manual, first.manual);
        assert_eq!(back.skills, first.skills);
        assert_eq!(back.persona.instructions, first.persona.instructions);
        assert_eq!(
            back.extras["hermes"]["config"],
            first.extras["hermes"]["config"]
        );
    }
}

#[test]
fn hermes_write_does_not_duplicate_a_kept_mcp_server_or_list_it_as_manual() {
    let src = scratch("mcp-src");
    std::fs::write(
        src.join("config.yaml"),
        "mcp_servers:\n  My_Server:\n    url: https://x.example.test/mcp\n  local-tool:\n    command: tool\n",
    )
    .unwrap();
    let rounds = hermes_rounds("rounds-mcp", &src, 2);
    for back in &rounds {
        let mut names: Vec<_> = back.connectors.iter().map(|c| c.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["My_Server", "local-tool"]);
        assert!(back.manual.is_empty(), "{:?}", back.manual);
    }
    std::fs::remove_dir_all(&src).unwrap();
}

#[test]
fn hermes_manual_names_with_colons_and_ticks_survive() {
    let src = scratch("manual-src");
    std::fs::write(
        src.join("botport-manual.md"),
        "# Set up by hand (botport)\n\n\
         - [ ] schedule Report: weekly: no schedule/timezone in the description: create the Hermes cron job by hand\n\
         - [x] connector Team: wiki: builtin connector: enable the matching Hermes tool or MCP server by hand\n\
         - [ ] schedule A: b: not imported: looks like it holds a secret (x); its text was not copied, set it up by hand\n\
         - [x] skill odd: hand-written todo\n",
    )
    .unwrap();
    let rounds = hermes_rounds("rounds-manual", &src, 2);
    let got = |m: &BotModel| {
        let mut v: Vec<_> = m
            .manual
            .iter()
            .map(|x| {
                (
                    x.item.clone(),
                    x.todo.split(' ').next().unwrap().to_string(),
                    x.done,
                )
            })
            .collect();
        v.sort();
        v
    };
    let want = vec![
        (
            "connector Team: wiki".to_string(),
            "builtin".to_string(),
            true,
        ),
        ("schedule A: b".into(), "not".into(), false),
        ("schedule Report: weekly".into(), "no".into(), false),
        ("skill odd".into(), "hand-written".into(), true),
    ];
    for m in &rounds {
        assert_eq!(got(m), want);
    }
    std::fs::remove_dir_all(&src).unwrap();
}

/// The conformance Hermes fixture read, `edit` applied to the model, then write -> read: the connectors read back.
fn hermes_edit_round(
    tag: &str,
    edit: impl Fn(&mut BotModel),
) -> Vec<crate::core::model::Connector> {
    let mut m = HermesProfile
        .read(&root().join("tests/fixtures/conformance/hermes"))
        .unwrap();
    edit(&mut m);
    let d = scratch(tag);
    HermesProfile.write(&m, &d).unwrap();
    let mut back = HermesProfile.read(&d).unwrap().connectors;
    back.sort_by(|a, b| a.name.cmp(&b.name));
    std::fs::remove_dir_all(&d).unwrap();
    back
}

fn http(name: &str, url: &str, tools: &[&str]) -> crate::core::model::Connector {
    crate::core::model::Connector {
        name: name.into(),
        kind: "mcp-http".into(),
        url: Some(url.into()),
        tools: tools.iter().map(|t| t.to_string()).collect(),
    }
}

#[test]
fn hermes_write_takes_an_edited_kept_connector_from_the_model() {
    let back = hermes_edit_round("mcp-edit", |m| {
        m.connectors[0].url = Some("https://new.example.test/mcp".into());
        m.connectors[0].tools = vec!["sign_in".into()];
    });
    assert_eq!(
        back,
        [http(
            "visitor-log",
            "https://new.example.test/mcp",
            &["sign_in"]
        )]
    );
    // dropping the tools list removes the kept one
    let back = hermes_edit_round("mcp-edit-notools", |m| m.connectors[0].tools.clear());
    assert_eq!(back[0].tools, Vec::<String>::new());
}

#[test]
fn hermes_write_keeps_a_second_server_at_the_same_url() {
    let back = hermes_edit_round("mcp-same-url", |m| {
        let url = m.connectors[0].url.clone().unwrap();
        m.connectors.push(http("visitor-admin", &url, &["sign_in"]));
    });
    let names: Vec<_> = back
        .iter()
        .map(|c| (c.name.as_str(), c.tools.clone()))
        .collect();
    assert_eq!(
        names,
        [
            ("visitor-admin", vec!["sign_in".to_string()]),
            ("visitor-log", vec!["list_visitors".to_string()])
        ]
    );
}

#[test]
fn hermes_write_does_not_merge_a_new_connector_into_a_kept_one_with_its_slug() {
    let back = hermes_edit_round("mcp-slug", |m| {
        m.connectors
            .push(http("Visitor Log", "https://b.example.test/mcp", &[]));
    });
    let got: Vec<_> = back
        .iter()
        .map(|c| (c.name.as_str(), c.url.as_deref().unwrap(), c.tools.len()))
        .collect();
    assert_eq!(
        got,
        [
            ("visitor-log", "https://visitors.example.test/mcp", 1),
            ("visitor-log-2", "https://b.example.test/mcp", 0)
        ]
    );
}

#[test]
fn hermes_ticked_credential_lines_stay_ticked() {
    let src = scratch("cred-src");
    let line = "credential FOO_TOKEN: set it on the Hermes side by hand (e.g. the profile's .env); no value was copied";
    std::fs::write(
        src.join("botport-manual.md"),
        format!("# Set up by hand (botport)\n\n- [x] {line}\n- [ ] credential BAR_TOKEN: set it on the Hermes side by hand (e.g. the profile's .env); no value was copied\n"),
    )
    .unwrap();
    for (i, m) in hermes_rounds("rounds-cred", &src, 2).iter().enumerate() {
        assert_eq!(m.credentials.len(), 2);
        if i > 0 {
            let d = scratch("cred-out");
            HermesProfile.write(m, &d).unwrap();
            let text = std::fs::read_to_string(d.join("botport-manual.md")).unwrap();
            assert!(text.contains(&format!("- [x] {line}")), "{text}");
            assert!(text.contains("- [ ] credential BAR_TOKEN"), "{text}");
            std::fs::remove_dir_all(&d).unwrap();
        }
    }
    std::fs::remove_dir_all(&src).unwrap();
}

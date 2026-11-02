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

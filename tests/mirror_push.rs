//! `botport mirror push`: the Mac-side check and one batched ask to the Mirror bot, on the offline fake Grok
//! transport. Invented pairs demo-bot-a/b/c; nothing touches a real app, profile or ~/.botport.
use std::path::{Path, PathBuf};
use std::process::Command;

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("mirror-push-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config/botport/mirror")).unwrap();
        for p in ["demo-bot-a", "demo-bot-b", "demo-bot-c"] {
            let prof = root.join("profiles").join(p);
            std::fs::create_dir_all(&prof).unwrap();
            std::fs::write(prof.join("botport.mirror.json"), "{\"gen\":1}").unwrap();
            let cfg = format!(
                "pair = \"{p}\"\nprofile_dir = \"{}\"\nprofile = \"{p}\"\nhermes_bin = \"hermes\"\ncron = false\n\
                 transport = \"local\"\nmachine = \"demo-mac\"\ncategories = []\napprove = \"all\"\n",
                prof.display()
            );
            std::fs::write(root.join(format!("config/botport/mirror/{p}.toml")), cfg).unwrap();
        }
        Env { root }
    }

    fn touch(&self, pair: &str, gen: u32) {
        let f = self
            .root
            .join("profiles")
            .join(pair)
            .join("botport.mirror.json");
        std::fs::write(f, format!("{{\"gen\":{gen}}}")).unwrap();
    }

    fn push(&self, bot: &str) -> (i32, serde_json::Value) {
        let fx = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("crates/botport-grok/tests/fixtures/app-1.1.0");
        let o = Command::new(env!("CARGO_BIN_EXE_botport"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("BOTPORT_HOME", self.root.join("botport-home"))
            .env_remove("BOTPORT_GROK_FAKE")
            .args(["mirror", "push", "--bot", bot, "--grok-fixtures"])
            .arg(&fx)
            .arg("--json")
            .output()
            .unwrap();
        let v = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| {
            serde_json::json!({"stdout": String::from_utf8_lossy(&o.stdout), "stderr": String::from_utf8_lossy(&o.stderr)})
        });
        (o.status.code().unwrap_or(-1), v)
    }
}

fn turns(v: &serde_json::Value) -> i64 {
    v["turns"].as_i64().unwrap_or(-1)
}

#[test]
fn no_change_is_zero_turns_and_one_change_in_each_of_3_pairs_is_one_ask() {
    let e = Env::new("batch");
    // first push: every pair is new to the push, one ask for all three
    let (code, out) = e.push("Mirror Bot");
    assert_eq!(code, 0, "{out}");
    assert_eq!(turns(&out), 1, "{out}");
    // nothing changed: no Grok turn; a bot that does not exist proves the engine is never asked
    let (code, out) = e.push("No Such Bot");
    assert_eq!(code, 0, "{out}");
    assert_eq!(turns(&out), 0, "{out}");
    assert_eq!(out["changed"], serde_json::json!([]), "{out}");
    // one change in each of the three pairs: one batched ask naming all of them
    for p in ["demo-bot-a", "demo-bot-b", "demo-bot-c"] {
        e.touch(p, 2);
    }
    let (code, out) = e.push("Mirror Bot");
    assert_eq!(code, 0, "{out}");
    assert_eq!(turns(&out), 1, "{out}");
    assert_eq!(
        out["changed"],
        serde_json::json!(["demo-bot-a", "demo-bot-b", "demo-bot-c"]),
        "{out}"
    );
    assert_eq!(turns(&e.push("No Such Bot").1), 0);
}

#[test]
fn a_failed_ask_is_not_marked_pushed() {
    let e = Env::new("fail");
    let (code, out) = e.push("No Such Bot");
    assert_ne!(code, 0, "{out}");
    // still pending: the next push asks again
    let (code, out) = e.push("Mirror Bot");
    assert_eq!((code, turns(&out)), (0, 1), "{out}");
}

#[test]
fn a_quiet_sync_restamping_the_state_is_no_change() {
    let e = Env::new("quiet");
    let f = |pair: &str| {
        e.root
            .join("profiles")
            .join(pair)
            .join("botport.mirror.json")
    };
    let st = |at: &str, seq: u32| {
        format!("{{\"gen\":1,\"remote_seq\":{seq},\"items\":{{\"skill:x\":{{\"base_local\":\"d1\",\"at\":\"{at}\"}}}}}}")
    };
    std::fs::write(f("demo-bot-a"), st("2026-11-17T00:00:00Z", 1)).unwrap();
    assert_eq!(turns(&e.push("Mirror Bot").1), 1);
    // a later sync that found nothing new: only the time stamps and the bot's sequence number moved
    std::fs::write(f("demo-bot-a"), st("2026-11-17T00:05:00Z", 2)).unwrap();
    let (code, out) = e.push("No Such Bot");
    assert_eq!((code, turns(&out)), (0, 0), "{out}");
    // a real change of an item still asks
    std::fs::write(
        f("demo-bot-a"),
        st("2026-11-17T00:10:00Z", 2).replace("d1", "d2"),
    )
    .unwrap();
    assert_eq!(turns(&e.push("Mirror Bot").1), 1);
}

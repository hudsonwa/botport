//! The CLI exit-code contract, checked through the built binary (offline: `--fixtures` fake transport only).
//!
//! `botport` (bridge commands, src/main.rs):
//! - 0: done (`--version`/`-V`, `--help`/`-h`/`help`, `version --json`, a command that succeeded);
//! - 1: the command ran and found something (drift: a difference; doctor / mirror doctor: a failed check);
//! - 2: usage error, nothing ran (no command, an unknown flag or command, a missing required option, a
//!   `--check-routines` on a command that does not take it), with `botport: ERROR:` on stderr.
//!
//! `botport grok <command>` (crates/botport-grok, docs/GROK-ENGINE-INTERFACES.md §1):
//! - 0 done; 1 other failure (bad arguments, unknown bot); 2 timeout (the bot is still working);
//!   3 waiting for the user (an approval card); 4 refused by policy (a floor); 5 app/transport unavailable;
//! - `grok --help` and `grok help` print the usage and exit 0.
#![cfg(feature = "grok")]
use std::path::{Path, PathBuf};
use std::process::Command;

struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("botport-exit-codes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Home(p)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fx() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/botport-grok/tests/fixtures/app-1.1.0")
}

fn run(h: &Path, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_botport"))
        .args(args)
        .env("BOTPORT_HOME", h.join("botport"))
        .env("HOME", h)
        .env("XDG_STATE_HOME", h.join("state"))
        .env("HERMES_HOME", h.join("hermes"))
        .env_remove("BOTPORT_GROK_FAKE")
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into(),
        String::from_utf8_lossy(&o.stderr).into(),
    )
}

#[test]
fn bridge_cli_exit_codes() {
    let h = Home::new("bridge");
    for a in [
        &["--version"][..],
        &["-V"],
        &["--help"],
        &["-h"],
        &["help"],
        &["version", "--json"],
    ] {
        let (rc, out, err) = run(&h.0, a);
        assert_eq!(rc, 0, "{a:?}: {err}");
        assert!(!out.is_empty(), "{a:?} prints to stdout");
    }
    for a in [
        &[][..],
        &["--bogus-flag"],
        &["frobnicate"],
        &["plan"],
        &["digest"],
        &["export", "--check-routines", "x"],
    ] {
        let (rc, out, err) = run(&h.0, a);
        assert_eq!(rc, 2, "{a:?}: usage error; stdout {out:?}, stderr {err:?}");
        assert!(!err.is_empty(), "{a:?}: the reason goes to stderr");
    }
    let (rc, _, err) = run(&h.0, &["--bogus-flag"]);
    assert_eq!(rc, 2);
    assert!(err.starts_with("botport: ERROR:"), "{err}");
}

#[test]
fn grok_cli_exit_codes_cover_every_code() {
    let h = Home::new("grok");
    let f = fx();
    let f = f.to_str().unwrap();
    let bot2 = "00000000-0000-4000-8000-000000000002";
    let g = |a: &[&str]| {
        let mut v = vec!["grok"];
        v.extend_from_slice(a);
        run(&h.0, &v)
    };
    // 0: done, and help
    for a in [
        &["--help"][..],
        &["help"],
        &["bots", "--fixtures", f],
        &["read", "main", "--limit", "1", "--fixtures", f],
    ] {
        let (rc, _, err) = g(a);
        assert_eq!(rc, 0, "{a:?}: {err}");
    }
    let (rc, out, _) = g(&["help"]);
    assert_eq!(rc, 0);
    assert!(
        out.contains("exit: 0 done"),
        "grok help prints the engine usage: {out}"
    );
    // 1: other failure
    for a in [
        &["bots", "--bogus"][..],
        &["read", "no-such-bot", "--fixtures", f],
        &["files", "--fixtures", f],
    ] {
        assert_eq!(g(a).0, 1, "{a:?}");
    }
    // 2: still working (nothing queued, so the wait times out)
    assert_eq!(
        g(&["ask", "main", "hi", "--fixtures", f, "--timeout", "2"]).0,
        2
    );
    // 3: waiting for the user (the main bot shows an approval card)
    let aw = h.0.join("awaiting");
    std::fs::create_dir_all(aw.join("turns")).unwrap();
    for sub in ["", "turns"] {
        for e in std::fs::read_dir(fx().join(sub)).unwrap().flatten() {
            if e.path().is_file() {
                std::fs::copy(e.path(), aw.join(sub).join(e.file_name())).unwrap();
            }
        }
    }
    std::fs::copy(
        aw.join("transcript-awaiting-approval.json"),
        aw.join("transcript-idle.json"),
    )
    .unwrap();
    let (rc, out, _) = g(&[
        "wait",
        "main",
        "--fixtures",
        aw.to_str().unwrap(),
        "--timeout",
        "2",
    ]);
    assert_eq!(rc, 3, "{out}");
    assert!(
        out.contains("floor 2"),
        "names the card it will not answer: {out}"
    );
    // 4: refused by policy (a non-exact name for a delete: floor 4)
    let (rc, out, _) = g(&["files", bot2, "delete", "export", "--fixtures", f]);
    assert_eq!(rc, 4, "{out}");
    // 5: transport unavailable (the fake transport without its fixtures)
    assert_eq!(g(&["bots", "--transport", "fake"]).0, 5);
    // --json carries the same exit in the body
    let (rc, out, _) = g(&["files", bot2, "delete", "export", "--fixtures", f, "--json"]);
    assert_eq!(rc, 4);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["exit"], 4);
}

//! One default home: `g2h home` and `g2hd --home` agree with an empty
//! env, `G2H_HOME` overrides, a legacy home is migrated, a symlinked home works.

use std::path::{Path, PathBuf};
use std::process::Command;

fn home_of(bin: &str, arg: &str, home: &Path, g2h_home: Option<&Path>) -> (bool, String) {
    let mut c = Command::new(bin);
    c.env_clear().env("HOME", home).arg(arg);
    if let Some(g) = g2h_home {
        c.env("G2H_HOME", g);
    }
    let o = c.output().unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).trim().to_string(),
    )
}

fn both(home: &Path, g2h_home: Option<&Path>) -> (String, String) {
    let (ok1, a) = home_of(env!("CARGO_BIN_EXE_g2h"), "home", home, g2h_home);
    let (ok2, b) = home_of(env!("CARGO_BIN_EXE_g2hd"), "--home", home, g2h_home);
    assert!(ok1 && ok2, "{a} / {b}");
    (a, b)
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

#[test]
fn cli_and_daemon_agree_on_dot_g2h_with_empty_env() {
    let t = tempfile::tempdir().unwrap();
    let (a, b) = both(t.path(), None);
    assert_eq!(a, b);
    assert_eq!(PathBuf::from(&a), t.path().join(".g2h"));
    std::fs::create_dir_all(t.path().join(".g2h")).unwrap();
    let (a, b) = both(t.path(), None);
    assert_eq!(a, b);
    assert_eq!(PathBuf::from(&a), canon(&t.path().join(".g2h")));
}

#[test]
fn g2h_home_overrides() {
    let t = tempfile::tempdir().unwrap();
    let g = t.path().join("elsewhere");
    let (a, b) = both(t.path(), Some(&g));
    assert_eq!(
        (a.as_str(), b.as_str()),
        (g.to_str().unwrap(), g.to_str().unwrap())
    );
}

#[test]
fn legacy_home_is_migrated_by_rename() {
    let t = tempfile::tempdir().unwrap();
    let legacy = if cfg!(target_os = "macos") {
        t.path().join("Library/Application Support/g2h")
    } else {
        t.path().join(".local/state/g2h")
    };
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("config.toml"), "peers = [\"grok\"]\n").unwrap();
    let (a, b) = both(t.path(), None);
    assert_eq!(a, b);
    assert_eq!(PathBuf::from(&a), canon(&t.path().join(".g2h")));
    assert!(t.path().join(".g2h/config.toml").exists());
    assert!(!legacy.exists());
}

#[test]
fn symlinked_home_works_for_inbox_credentials() {
    let t = tempfile::tempdir().unwrap();
    let real = t.path().join(".g2h");
    std::fs::create_dir_all(&real).unwrap();
    let link = t.path().join("linked");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_g2h"))
        .env_clear()
        .env("HOME", t.path())
        .env("G2H_HOME", &link)
        .args(["setup", "--inbox-credentials"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    c.stdin
        .take()
        .unwrap()
        .write_all(b"https://hooks.example/x\nkey-0123456789abc\n")
        .unwrap();
    let o = c.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(real.join("inbox.credentials").exists());
}

#[test]
fn g2h_home_outside_home_warns() {
    // An inherited G2H_HOME (e.g. exported from a shell rc) survives `export HOME=$(mktemp -d)`.
    let t = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_g2h"))
        .env_clear()
        .env("HOME", t.path())
        .env("G2H_HOME", elsewhere.path().join(".g2h"))
        .arg("home")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("G2H_HOME") && err.contains("outside"), "{err}");
    let (_, inside) = home_of(
        env!("CARGO_BIN_EXE_g2h"),
        "home",
        t.path(),
        Some(&t.path().join("x")),
    );
    assert!(!inside.is_empty());
}

#[test]
fn g2h_home_inside_fresh_home_does_not_warn() {
    // The documented try-out: HOME=$(mktemp -d) G2H_HOME=$HOME/.g2h, with .g2h not created yet.
    let t = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_g2h"))
        .env_clear()
        .env("HOME", t.path())
        .env("G2H_HOME", t.path().join(".g2h"))
        .arg("home")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!err.contains("outside"), "{err}");
}

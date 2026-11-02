//! End-to-end: the real `g2hd` and `g2h` binaries against the fake gateway.
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn g2h(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_g2h"))
        .env("G2H_HOME", home)
        .args(args)
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn spawn_daemon(home: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_g2hd"))
        .env("G2H_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for(p: &Path) {
    let t = Instant::now();
    while !p.exists() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_verbs_end_to_end_with_real_binaries() {
    let fg = support::FakeGateway::start().await;
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("g2h");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "backend = \"gateway\"\nhermes_home = \"{}\"\n",
            fg.path().display()
        ),
    )
    .unwrap();
    let home2 = home.clone();
    tokio::task::spawn_blocking(move || {
        let home = home2.as_path();
        // doctor before the daemon runs: says so, with a hint
        let o = g2h(home, &["doctor"]);
        assert!(!o.status.success());
        assert!(out(&o).contains("start g2hd"), "{}", out(&o));

        let mut d = spawn_daemon(home);
        wait_for(&home.join("g2hd.sock"));

        // second instance exits with a clear message
        let o2 = Command::new(env!("CARGO_BIN_EXE_g2hd"))
            .env("G2H_HOME", home)
            .output()
            .unwrap();
        assert!(!o2.status.success());
        assert!(String::from_utf8_lossy(&o2.stderr).contains("already running"));

        // no listening inet socket
        let pid = d.id().to_string();
        // (skipped where lsof is not installed, e.g. slim Linux images)
        match Command::new("lsof")
            .args(["-nP", "-a", "-p", &pid, "-i"])
            .output()
        {
            Ok(lsof) => {
                let l = out(&lsof);
                assert!(!l.contains("LISTEN"), "g2hd must not listen on TCP: {l}");
            }
            Err(e) => eprintln!("lsof unavailable ({e}); TCP listen check skipped"),
        }

        let o = g2h(home, &["ask", "hello there", "--timeout", "10"]);
        assert!(o.status.success(), "{o:?}");
        assert_eq!(
            out(&o).trim(),
            g2h::framing::frame("hermes", "pong"),
            "ask output is framed"
        );
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("first_delta_ms=") && err.contains("total_ms="),
            "{err}"
        );

        let o = g2h(home, &["ask", "x", "--timeout", "721"]);
        assert!(!o.status.success());

        let o = g2h(home, &["send", "fire and forget"]);
        assert!(o.status.success());
        assert_eq!(out(&o).trim().len(), 36, "msg_id: {}", out(&o));

        let o = g2h(home, &["wait", "--timeout", "5"]);
        assert_eq!(o.status.code(), Some(0));
        assert!(out(&o).contains("pong"));

        let o = g2h(home, &["wait", "--timeout", "1"]);
        assert_eq!(o.status.code(), Some(2), "timeout exit code");

        let o = g2h(home, &["hermes-send", "hello grok", "--conv", "k1"]);
        assert!(o.status.success(), "{o:?}");
        let o = g2h(home, &["recv", "--json"]);
        let v: serde_json::Value = serde_json::from_str(out(&o).trim()).unwrap();
        assert_eq!(
            (v["body"].as_str(), v["conv_id"].as_str()),
            (Some("hello grok"), Some("k1"))
        );
        let o = g2h(home, &["recv"]);
        assert_eq!(o.status.code(), Some(2), "empty inbox");

        let o = g2h(home, &["status"]);
        let s = out(&o);
        assert!(
            o.status.success() && s.contains("dry_run: true") && s.contains("grok_active: true"),
            "{s}"
        );
        assert!(s.contains("webhook credentials: missing"));
        assert!(g2h(home, &["bye"]).status.success());
        assert!(out(&g2h(home, &["status"])).contains("grok_active: false"));

        let o = g2h(home, &["doctor"]);
        assert!(o.status.success(), "{}", out(&o));
        assert!(out(&o).contains("backend"));

        // graceful shutdown on SIGTERM
        Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        let t = Instant::now();
        loop {
            if let Some(st) = d.try_wait().unwrap() {
                assert!(st.success(), "{st:?}");
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!home.join("g2hd.sock").exists());
    })
    .await
    .unwrap();
}

#[test]
fn approve_needs_a_code() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("g2h");
    let o = g2h(&home, &["approve", "0190abcdef123456"]);
    assert!(!o.status.success());
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("approve needs <request_id> <code>"), "{e}");
}

/// With no daemon, `doctor` runs local checks only: exit 1, fix hints, no paths
/// outside the given home and no macOS-only advice.
#[test]
fn doctor_without_daemon_is_machine_neutral() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("g2h-home");
    let o = g2h(&home, &["doctor"]);
    let s = out(&o);
    assert_eq!(o.status.code(), Some(1), "{s}");
    assert!(s.contains("[FAIL] daemon") && s.contains("fix:"), "{s}");
    assert!(s.contains("systemd") && s.contains("g2hd &"), "{s}");
    assert!(s.contains("[ok] home: not created yet"), "{s}");
    assert!(!s.contains("Library/"), "{s}");
}

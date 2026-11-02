//! `g2h setup` / `--check` / `uninstall` / `selftest` / `report` / `wake-reset`
//! with a fake `hermes` on PATH, a temp G2H_HOME and the fake gateway.
mod support;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const URL: &str = "http://127.0.0.1:9/hooks/SECRETPATH-abc";
const KEY: &str = "SENDERKEY-zzz-0123456789";

struct Env {
    _t: tempfile::TempDir,
    home: PathBuf,
    hermes: PathBuf,
    bin: PathBuf,
    svc: PathBuf,
}

fn env() -> Env {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("g2h");
    let hermes = t.path().join("hermes-home");
    std::fs::create_dir_all(hermes.join("profiles/alpha")).unwrap();
    std::fs::create_dir_all(hermes.join("profiles/beta")).unwrap();
    let bin = t.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("hermes");
    std::fs::write(&fake, "#!/bin/sh\necho 'Hermes Agent v9.9.9'\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let svc = t.path().join("services");
    Env {
        home,
        hermes,
        bin,
        svc,
        _t: t,
    }
}

fn g2h(e: &Env, args: &[&str], stdin: &str) -> Output {
    let path = format!("{}:/usr/bin:/bin", e.bin.display());
    let mut c = Command::new(env!("CARGO_BIN_EXE_g2h"))
        .env("G2H_HOME", &e.home)
        .env("PATH", path)
        .env("G2H_SERVICE_DIR", &e.svc)
        .env("G2H_NO_SERVICE_LOAD", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    c.wait_with_output().unwrap()
}

fn all(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn setup(e: &Env) -> Output {
    let h = e.hermes.to_str().unwrap();
    g2h(
        e,
        &["setup", "--hermes-home", h, "--profile", "alpha", "--yes"],
        &format!("{URL}\n{KEY}\n"),
    )
}

fn assert_no_secret(s: &str) {
    assert!(
        !s.contains(KEY) && !s.contains("SECRETPATH"),
        "secret leaked: {s}"
    );
}

fn service_file(e: &Env) -> PathBuf {
    if cfg!(target_os = "macos") {
        e.svc.join("io.g2h.daemon.plist")
    } else {
        e.svc.join("io.g2h.daemon.service")
    }
}

#[test]
fn setup_is_idempotent_and_writes_everything() {
    let e = env();
    let o = setup(&e);
    assert!(o.status.success(), "{}", all(&o));
    assert_no_secret(&all(&o));
    assert!(all(&o).contains("v9.9.9"), "{}", all(&o));
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    assert!(cfg.contains("profile = \"alpha\""), "{cfg}");
    let cred = e.home.join("credentials");
    assert_eq!(
        std::fs::metadata(&cred).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let c = std::fs::read_to_string(&cred).unwrap();
    assert!(c.contains("webhook_url=") && c.contains("sender_key=") && c.contains("key_mode="));
    let svc = std::fs::read_to_string(service_file(&e)).unwrap();
    assert!(svc.contains("g2hd") && svc.contains(e.home.to_str().unwrap()));
    // the Hermes-side step is printed, not done
    assert!(all(&o).contains("hermes-send"), "{}", all(&o));
    let m1 = std::fs::read_to_string(e.home.join("setup-manifest.json")).unwrap();

    // second run: no prompts needed (credentials already set), nothing changes
    let h = e.hermes.to_str().unwrap();
    let o = g2h(
        &e,
        &["setup", "--hermes-home", h, "--profile", "alpha", "--yes"],
        "",
    );
    assert!(o.status.success(), "{}", all(&o));
    assert_eq!(
        std::fs::read_to_string(e.home.join("setup-manifest.json")).unwrap(),
        m1
    );
    assert_eq!(
        std::fs::read_to_string(e.home.join("config.toml")).unwrap(),
        cfg
    );
    assert_eq!(std::fs::read_to_string(&cred).unwrap(), c);
}

#[test]
fn setup_needs_profile_choice_and_consent() {
    let e = env();
    let h = e.hermes.to_str().unwrap();
    // two profiles, none chosen: lists them, changes nothing
    let o = g2h(&e, &["setup", "--hermes-home", h, "--yes"], "");
    assert!(!o.status.success());
    assert!(all(&o).contains("alpha") && all(&o).contains("beta") && all(&o).contains("--profile"));
    assert!(!e.home.join("config.toml").exists());
    // no --yes and consent refused: nothing written
    let o = g2h(
        &e,
        &["setup", "--hermes-home", h, "--profile", "alpha"],
        "n\n",
    );
    assert!(!o.status.success(), "{}", all(&o));
    assert!(!e.home.join("config.toml").exists());
    // unknown profile
    let o = g2h(
        &e,
        &["setup", "--hermes-home", h, "--profile", "nope", "--yes"],
        "",
    );
    assert!(!o.status.success());
}

#[test]
fn check_shows_set_missing_never_secrets() {
    let e = env();
    let o = g2h(&e, &["setup", "--check"], "");
    assert!(!o.status.success());
    let s = all(&o);
    assert!(
        s.contains("credentials") && s.contains("missing") && s.contains("fix:"),
        "{s}"
    );
    assert!(setup(&e).status.success());
    let o = g2h(&e, &["setup", "--check"], "");
    let s = all(&o);
    assert_no_secret(&s);
    assert!(s.contains("[ok] credentials: set"), "{s}");
    assert!(s.contains("[ok] hermes"), "{s}");
    assert!(s.contains("[FAIL] daemon"), "{s}"); // not running in this test
}

#[test]
fn uninstall_reverses_the_manifest() {
    let e = env();
    assert!(setup(&e).status.success());
    std::fs::write(e.home.join("g2h.log.jsonl"), "{}\n").unwrap();
    let o = g2h(&e, &["uninstall", "--keep-logs"], "");
    assert!(o.status.success(), "{}", all(&o));
    let s = all(&o);
    assert_no_secret(&s);
    assert!(s.to_lowercase().contains("routine"), "{s}");
    for f in ["credentials", "config.toml", "setup-manifest.json"] {
        assert!(!e.home.join(f).exists(), "{f} left behind");
    }
    assert!(!service_file(&e).exists());
    assert!(e.home.join("g2h.log.jsonl").exists(), "--keep-logs");
    // a second uninstall is harmless
    assert!(g2h(&e, &["uninstall"], "").status.success());
    assert!(!e.home.join("g2h.log.jsonl").exists());
}

fn spawn_daemon(home: &Path) -> Child {
    let d = Command::new(env!("CARGO_BIN_EXE_g2hd"))
        .env("G2H_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while !home.join("g2hd.sock").exists() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    d
}

fn daemon_home(fg: &support::FakeGateway, e: &Env, creds_url: &str) {
    std::fs::create_dir_all(&e.home).unwrap();
    std::fs::set_permissions(&e.home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        e.home.join("config.toml"),
        format!("hermes_home = \"{}\"\n", fg.path().display()),
    )
    .unwrap();
    let c = e.home.join("credentials");
    std::fs::write(
        &c,
        format!(
            "webhook_url={creds_url}\nsender_key={KEY}\nkey_mode=header:Authorization-Bearer\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selftest_dry_run_against_fake_gateway() {
    let fg = support::FakeGateway::start().await;
    let e = env();
    daemon_home(&fg, &e, URL);
    tokio::task::spawn_blocking(move || {
        let mut d = spawn_daemon(&e.home);
        let o = g2h(&e, &["selftest"], "");
        let s = all(&o);
        assert!(o.status.success(), "{s}");
        assert!(
            s.contains("pong") && s.contains("ttfd_ms=") && s.contains("total_ms="),
            "{s}"
        );
        assert!(s.contains("dry-run wake"), "{s}");
        assert_no_secret(&s);
        // --live without consent fires nothing
        let o = g2h(&e, &["selftest", "--live"], "n\n");
        assert!(!o.status.success());
        let log = std::fs::read_to_string(e.home.join("g2h.log.jsonl")).unwrap();
        assert!(!log.contains("\"live\":true"), "{log}");
        d.kill().unwrap();
        d.wait().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selftest_live_round_trip_via_report() {
    let fg = support::FakeGateway::start().await;
    let e = env();
    // one-shot fake webhook: reads the POST, answers 200, then plays the Grok side: `g2h report <token>`
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/hook", l.local_addr().unwrap().port());
    daemon_home(&fg, &e, &url);
    let home = e.home.clone();
    let hook = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = s.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let t = String::from_utf8_lossy(&buf);
            if t.contains("\r\n\r\n") && t.trim_end().ends_with('}') {
                break;
            }
        }
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        let req = String::from_utf8_lossy(&buf).to_string();
        let token = req
            .split("g2h report ")
            .nth(1)
            .unwrap()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect::<String>();
        let o = Command::new(env!("CARGO_BIN_EXE_g2h"))
            .env("G2H_HOME", &home)
            .args(["report", &token])
            .output()
            .unwrap();
        assert!(o.status.success(), "{o:?}");
        req
    });
    tokio::task::spawn_blocking(move || {
        let mut d = spawn_daemon(&e.home);
        let o = g2h(&e, &["selftest", "--live", "--yes", "--timeout", "20"], "");
        let s = all(&o);
        assert!(o.status.success(), "{s}");
        assert!(s.contains("wake->reply"), "{s}");
        assert_no_secret(&s);
        let req = hook.join().unwrap();
        assert!(req.contains(&format!("Bearer {KEY}")));
        d.kill().unwrap();
        d.wait().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_alert_cleared_on_restart_and_by_wake_reset() {
    let fg = support::FakeGateway::start().await;
    let e = env();
    daemon_home(&fg, &e, URL);
    let st = g2h::store::State {
        alert: Some("webhook returned HTTP 401".into()),
        ..Default::default()
    };
    st.save(&e.home.join("state.json")).unwrap();
    tokio::task::spawn_blocking(move || {
        let mut d = spawn_daemon(&e.home);
        let s = all(&g2h(&e, &["status"], ""));
        assert!(!s.contains("ALERT"), "restart must clear the alert: {s}");
        let o = g2h(&e, &["wake-reset"], "");
        assert!(o.status.success(), "{}", all(&o));
        d.kill().unwrap();
        d.wait().unwrap();
    })
    .await
    .unwrap();
}

/// Committed docs, sources, scripts and workflows carry no private paths, tailnet IPs or hosts.
#[test]
fn no_private_paths_in_docs_or_sources() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pats = [
        regex::Regex::new(&["/Us", "ers/"].concat()).unwrap(),
        regex::Regex::new(&[r"\b10", r"0\.\d{1,3}\.\d{1,3}\.\d{1,3}\b"].concat()).unwrap(),
        regex::Regex::new(&[r"\.ts", r"\.net\b"].concat()).unwrap(),
    ];
    let mut bad = Vec::new();
    let mut stack = vec![
        root.join("docs"),
        root.join("crates/g2h"),
        root.join("src"),
        root.join("scripts"),
        root.join(".github"),
        root.join("install.sh"),
        root.join("README.md"),
    ];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            if p.ends_with("target") {
                continue;
            }
            for d in std::fs::read_dir(&p).unwrap() {
                stack.push(d.unwrap().path());
            }
        } else if let Ok(t) = std::fs::read_to_string(&p) {
            for (n, line) in t.lines().enumerate() {
                if pats.iter().any(|r| r.is_match(line)) {
                    bad.push(format!("{}:{}", p.display(), n + 1));
                }
            }
        }
    }
    assert!(bad.is_empty(), "private paths/IPs/hosts: {bad:?}");
}

#[test]
fn uninstall_keeps_a_config_setup_did_not_create() {
    let e = env();
    std::fs::create_dir_all(&e.home).unwrap();
    std::fs::set_permissions(&e.home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(e.home.join("config.toml"), "hop_limit = 2\n").unwrap();
    assert!(setup(&e).status.success());
    let o = g2h(&e, &["uninstall"], "");
    assert!(o.status.success(), "{}", all(&o));
    let s = all(&o);
    assert!(
        e.home.join("config.toml").exists(),
        "pre-existing config removed"
    );
    assert!(s.contains("kept") && s.contains("config.toml"), "{s}");
    assert!(s.contains("removed") && s.contains("credentials"), "{s}");
}

#[test]
fn setup_reset_credentials_reprompts_and_overwrites() {
    let e = env();
    assert!(setup(&e).status.success());
    let h = e.hermes.to_str().unwrap();
    let new_url = "https://example.invalid/hooks/NEWPATH";
    let o = g2h(
        &e,
        &[
            "setup",
            "--hermes-home",
            h,
            "--profile",
            "alpha",
            "--yes",
            "--reset-credentials",
        ],
        &format!("{new_url}\nNEWKEY-0123456789\n"),
    );
    assert!(o.status.success(), "{}", all(&o));
    let s = all(&o);
    assert!(!s.contains("NEWKEY") && !s.contains("NEWPATH"), "{s}");
    let c = std::fs::read_to_string(e.home.join("credentials")).unwrap();
    assert!(c.contains(new_url) && c.contains("NEWKEY-0123456789"));
    assert!(!c.contains(KEY));
}

#[test]
fn setup_requires_https_except_loopback() {
    let h_url = |u: &str| {
        let e = env();
        let h = e.hermes.to_str().unwrap().to_string();
        let o = g2h(
            &e,
            &["setup", "--hermes-home", &h, "--profile", "alpha", "--yes"],
            &format!("{u}\n{KEY}\n"),
        );
        (o.status.success(), all(&o))
    };
    let (ok, s) = h_url("http://example.com/hooks/x");
    assert!(!ok, "{s}");
    assert!(s.contains("https://") && s.contains("loopback"), "{s}");
    for u in [
        "https://example.com/hooks/x",
        "http://127.0.0.1:9/h",
        "http://localhost:9/h",
        "http://[::1]:9/h",
    ] {
        let (ok, s) = h_url(u);
        assert!(ok, "{u}: {s}");
    }
    let (ok, s) = h_url("ftp://example.com/x");
    assert!(!ok, "{s}");
}

#[test]
fn url_and_key_prompts_reprompt_on_empty_or_invalid() {
    let e = env();
    let o = g2h(
        &e,
        &["setup", "--inbox-credentials"],
        &format!("\nftp://nope/x\n{URL}\n\n{KEY}\n"),
    );
    let s = all(&o);
    assert!(o.status.success(), "{s}");
    assert!(s.contains("empty"), "{s}");
    assert!(s.contains("https://"), "{s}");
    assert!(!s.contains(KEY), "{s}");
    let c = std::fs::read_to_string(e.home.join("inbox.credentials")).unwrap();
    assert!(c.contains(URL) && c.contains(KEY));
}

#[test]
fn prompts_give_up_after_three_tries() {
    let e = env();
    let o = g2h(
        &e,
        &["setup", "--inbox-credentials"],
        "\n\n\nhttps://late.example/x\nk\n",
    );
    let s = all(&o);
    assert!(!o.status.success(), "{s}");
    assert!(s.contains("3 tries"), "{s}");
    assert!(!e.home.join("inbox.credentials").exists());
    // the main wizard re-prompts too
    let h = e.hermes.to_str().unwrap();
    let o = g2h(
        &e,
        &["setup", "--hermes-home", h, "--profile", "alpha", "--yes"],
        &format!("\n{URL}\n{KEY}\n"),
    );
    assert!(o.status.success(), "{}", all(&o));
}

#[test]
fn setup_no_service_writes_config_and_credentials_only() {
    let e = env();
    let h = e.hermes.to_str().unwrap();
    let o = g2h(
        &e,
        &[
            "setup",
            "--hermes-home",
            h,
            "--profile",
            "alpha",
            "--yes",
            "--no-service",
        ],
        &format!("{URL}\n{KEY}\n"),
    );
    let s = all(&o);
    assert!(o.status.success(), "{s}");
    assert!(e.home.join("config.toml").exists() && e.home.join("credentials").exists());
    assert!(!service_file(&e).exists(), "{s}");
    assert!(
        s.contains("g2hd"),
        "must say to run g2hd in the foreground: {s}"
    );
    assert_no_secret(&s);
}

#[test]
fn selftest_wake_only_needs_no_gateway() {
    let e = env();
    std::fs::create_dir_all(&e.home).unwrap();
    std::fs::set_permissions(&e.home, std::fs::Permissions::from_mode(0o700)).unwrap();
    // a Hermes home with no running gateway
    std::fs::write(
        e.home.join("config.toml"),
        format!("hermes_home = \"{}\"\n", e.hermes.display()),
    )
    .unwrap();
    let c = e.home.join("credentials");
    std::fs::write(
        &c,
        format!("webhook_url={URL}\nsender_key={KEY}\nkey_mode=header:Authorization-Bearer\n"),
    )
    .unwrap();
    std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut d = spawn_daemon(&e.home);
    let o = g2h(&e, &["selftest", "--wake-only"], "");
    let s = all(&o);
    d.kill().unwrap();
    d.wait().unwrap();
    assert!(o.status.success(), "{s}");
    assert!(
        s.contains("dry-run wake") && !s.contains("round trip"),
        "{s}"
    );
    assert_no_secret(&s);
}

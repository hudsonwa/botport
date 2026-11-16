//! Regression tests for the 0.3.0 engine review (REVIEW-engine-030): one section per finding. Synthetic fixtures
//! only.
mod common;
use botport_grok::engine::WaitOpts;
use botport_grok::mcp;
use botport_grok::policy::{Caller, Policy};
use botport_grok::transport::fake::FakeTransport;
use botport_grok::ExitCode;
use common::*;
use serde_json::{json, Value};

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
}

fn lines(out: Vec<u8>) -> Vec<Value> {
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ------------------------------------------------------------------ H1: MCP re-checks the port TTL

#[test]
fn h1_mcp_drops_an_expired_engine_and_rebuilds_it() {
    let mut built = 0;
    let mut make = || {
        built += 1;
        let c = clock();
        let f = FakeTransport::load(&fx(), c.clone()).unwrap();
        let mut e = engine_from(
            f,
            c.clone(),
            Policy::default(),
            Caller::Mcp {
                bot_callable: false,
            },
        );
        // the first engine's port has already expired; the rebuilt one has 10 minutes left
        e.deadline_ms = Some(c.now_ms() + if built == 1 { 0 } else { 600_000 });
        Ok(e)
    };
    let mut status = || json!({"exit": 0});
    let input: String = [
        call(1, "grok_bots", json!({})),
        call(2, "grok_bots", json!({})),
        call(3, "grok_bots", json!({})),
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect();
    let mut out = Vec::new();
    mcp::serve(&mut make, &mut status, input.as_bytes(), &mut out, false).unwrap();
    let r = lines(out);
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|x| x["result"]["isError"] == false), "{r:?}");
    assert_eq!(
        built, 2,
        "the expired engine was rebuilt once, the fresh one kept"
    );
}

#[test]
fn h1_mcp_drops_the_engine_when_the_port_file_changes() {
    let mut built = 0;
    let mut make = || {
        built += 1;
        let c = clock();
        let f = FakeTransport::load(&fx(), c.clone()).unwrap();
        Ok(engine_from(
            f,
            c,
            Policy::default(),
            Caller::Mcp {
                bot_callable: false,
            },
        ))
    };
    let mut status = || json!({"exit": 0});
    let mut checks = 0;
    // the second check sees a restored/relaunched app
    let mut fresh = |_: &botport_grok::Engine| {
        checks += 1;
        checks != 2
    };
    let input: String = (1..=4)
        .map(|i| format!("{}\n", call(i, "grok_bots", json!({}))))
        .collect();
    let mut out = Vec::new();
    mcp::serve_checked(
        &mut make,
        &mut status,
        &mut fresh,
        input.as_bytes(),
        &mut out,
        false,
    )
    .unwrap();
    assert_eq!(lines(out).len(), 4);
    assert_eq!(built, 2);
}

#[test]
fn h1_port_file_must_match_pid_and_expiry() {
    use botport_grok::cli::port_file_matches;
    use botport_grok::lifecycle::PortFile;
    let pf = PortFile {
        port: 54321,
        pid: 100,
        ws_path: "/devtools/browser/synthetic".into(),
        opened_at_ms: 1_000,
        ttl_s: 60,
    };
    assert!(port_file_matches(Some(&pf), Some(100), Some(61_000)));
    assert!(port_file_matches(Some(&pf), Some(100), None), "--extend");
    assert!(
        !port_file_matches(None, Some(100), Some(61_000)),
        "restored: no port file"
    );
    assert!(
        !port_file_matches(Some(&pf), Some(101), Some(61_000)),
        "another app process"
    );
    assert!(!port_file_matches(Some(&pf), None, None), "app not running");
    assert!(
        !port_file_matches(Some(&pf), Some(100), Some(1)),
        "a newer launch"
    );
}

#[test]
fn h1_ask_and_wait_stop_at_the_port_deadline() {
    let mut e = eng(&["long-reply"]);
    let start = e.clock.now_ms();
    e.deadline_ms = Some(start + 5_000);
    let o = e.ask(
        "main",
        "write a lot",
        &[],
        &WaitOpts {
            timeout_s: 300.0,
            settle_s: 8.0,
        },
    );
    assert_eq!(o.exit, ExitCode::StillWorking);
    assert!(
        o.proof.line().contains("port TTL reached"),
        "{}",
        o.proof.line()
    );
    assert!(o.value.unwrap().waited_s < 30.0);
    let o = e.wait(
        "main",
        None,
        &WaitOpts {
            timeout_s: 300.0,
            settle_s: 8.0,
        },
    );
    assert!(
        o.proof.line().contains("port TTL reached"),
        "{}",
        o.proof.line()
    );
}

// ------------------------------------------------------------------ H2: fetch never reads a bot-chosen local path

#[test]
fn h2_only_the_apps_own_asset_url_is_read_directly() {
    use botport_grok::policy::floors::attachment_url_ok;
    let ok = format!("file:///home/box/agent-data/agents/{MAIN}/assets/0a1b2c3d4e.json");
    assert!(attachment_url_ok(&ok, MAIN));
    assert!(attachment_url_ok(
        &format!("file:///home/box/agent-data/agents/{MAIN}/assets/synthetic-1"),
        MAIN
    ));
    for bad in [
        "file:///etc/hosts".to_string(),
        "file:///home/synthetic/.ssh/id_ed25519".to_string(),
        "/workspace/export.json".to_string(),
        format!("file:///home/box/agent-data/agents/{TEMP}/assets/0a1b.json"),
        format!("file:///home/box/agent-data/agents/{MAIN}/assets/../../../etc/hosts"),
        format!("file:///home/box/agent-data/agents/{MAIN}/assets/%2e%2e"),
        format!("file:///home/box/agent-data/agents/{MAIN}/assets/a.json?x=1"),
        format!("file:///home/box/agent-data/agents/{MAIN}/assets/sub/a.json"),
        format!("file:///home/box/agent-data/agents/{MAIN}/assets/.hidden"),
        format!("file:///home/box/agent-data/agents/{MAIN}/assets/a.tar.gz"),
        format!("https://example.invalid/agent-data/agents/{MAIN}/assets/a.json"),
    ] {
        assert!(!attachment_url_ok(&bad, MAIN), "{bad}");
    }
}

#[test]
fn h2_fetch_of_a_file_url_outside_the_asset_store_takes_the_preview_route() {
    let s = Scratch::new("h2fetch");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_attachment_url(
            MAIN,
            "hosts",
            "file:///etc/hosts",
            b"SYNTHETIC LOCAL FILE".to_vec(),
            Some(b"synthetic preview bytes".to_vec()),
        );
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let o = e.file_fetch(MAIN, None, &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(
        std::fs::read(s.0.join("hosts")).unwrap(),
        b"synthetic preview bytes"
    );

    // no preview: the fetch fails and nothing local is copied
    let s = Scratch::new("h2fetch2");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_attachment_url(
            MAIN,
            "hosts",
            "file:///etc/hosts",
            b"SYNTHETIC LOCAL FILE".to_vec(),
            None,
        );
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    assert_eq!(e.file_fetch(MAIN, None, &s.0).exit, ExitCode::Other);
    assert_eq!(std::fs::read_dir(&s.0).unwrap().count(), 0);
}

// ------------------------------------------------------------------ M3: every bot/app-text result is wrapped

#[test]
fn m3_bots_routines_files_and_usage_are_wrapped_under_note() {
    const BOT2: &str = "00000000-0000-4000-8000-000000000002";
    let mut p = Policy::default();
    p.set("reply_wrapping", "note").unwrap();
    for bot_callable in [false, true] {
        let c = clock();
        let f = FakeTransport::load(&fx(), c.clone()).unwrap();
        let mut e = engine_from(f, c, p.clone(), Caller::Mcp { bot_callable });
        let mut st = || json!({"exit": 0});
        for (tool, args) in [
            ("grok_bots", json!({})),
            ("grok_routines", json!({"bot": MAIN, "action": "list"})),
            ("grok_files", json!({"bot": BOT2, "action": "list"})),
            ("grok_usage", json!({})),
        ] {
            let (v, _) = mcp::call(&mut e, &mut st, tool, &args, bot_callable).unwrap();
            assert!(
                v["note"]
                    .as_str()
                    .is_some_and(|n| n.contains("not instructions")),
                "{tool}: {v}"
            );
            assert!(v["untrusted_bot_content"].is_object(), "{tool}: {v}");
        }
    }
    // fast (raw) keeps the old shape
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone()).unwrap();
    let mut e = engine_from(
        f,
        c,
        Policy::default(),
        Caller::Mcp {
            bot_callable: false,
        },
    );
    let mut st = || json!({"exit": 0});
    let (v, _) = mcp::call(&mut e, &mut st, "grok_bots", &json!({}), false).unwrap();
    assert!(v["bots"].is_array());
}

// ------------------------------------------------------------------ M4: MCP paths stay in the server's folder

#[test]
fn m4_mcp_paths_are_confined_to_the_servers_folder() {
    use botport_grok::mcp::confine;
    let s = Scratch::new("m4");
    let root = s.0.join("root");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/report.txt"), b"synthetic").unwrap();
    std::fs::write(s.0.join("outside.txt"), b"synthetic").unwrap();
    std::fs::create_dir_all(root.join(".ssh")).unwrap();
    std::fs::write(root.join(".ssh/id_synthetic"), b"synthetic").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(s.0.join("outside.txt"), root.join("link.txt")).unwrap();

    assert!(confine(&root, "sub/report.txt", true).is_ok());
    assert!(confine(&root, ".", false).is_ok());
    assert!(
        confine(&root, "new/dir", false).is_ok(),
        "an out folder may not exist yet"
    );
    for (p, file) in [
        ("../outside.txt", true),
        (".ssh/id_synthetic", true),
        ("sub/../../outside.txt", true),
        ("/etc/hosts", true),
        ("..", false),
        ("/tmp", false),
        (".hidden-out", false),
        ("sub", true),
    ] {
        assert!(confine(&root, p, file).is_err(), "{p}");
    }
    #[cfg(unix)]
    assert!(confine(&root, "link.txt", true)
        .unwrap_err()
        .contains("symlink"));
}

// An existing absolute file outside the MCP folder. On Windows "/etc/hosts" has no drive letter, resolves
// to a missing C:\etc\hosts and is refused as "not found" instead of "outside".
#[cfg(unix)]
const OUTSIDE_FILE: &str = "/etc/hosts";
#[cfg(windows)]
const OUTSIDE_FILE: &str = r"C:\Windows\win.ini";

#[test]
fn m4_mcp_upload_of_a_path_outside_the_folder_is_refused() {
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone()).unwrap();
    let mut e = engine_from(
        f,
        c,
        Policy::default(),
        Caller::Mcp {
            bot_callable: false,
        },
    );
    let mut st = || json!({"exit": 0});
    let r = mcp::call(
        &mut e,
        &mut st,
        "grok_files",
        &json!({"bot": MAIN, "action": "upload", "paths": [OUTSIDE_FILE]}),
        false,
    );
    assert!(r.unwrap_err().contains("outside"));
    let r = mcp::call(
        &mut e,
        &mut st,
        "grok_files",
        &json!({"bot": MAIN, "action": "download", "out": "../../.."}),
        false,
    );
    assert!(r.is_err());
}

#[cfg(unix)]
#[test]
fn m4_upload_refuses_a_symlink() {
    let s = Scratch::new("m4link");
    std::fs::write(s.0.join("real.txt"), b"synthetic").unwrap();
    std::os::unix::fs::symlink(s.0.join("real.txt"), s.0.join("link.txt")).unwrap();
    let mut e = eng(&[]);
    let o = e.file_upload(MAIN, &[s.0.join("link.txt")]);
    assert_eq!(o.exit, ExitCode::Refused, "{}", o.proof.line());
    assert!(o.proof.line().contains("symlink"));
}

// ------------------------------------------------------------------ M5: no partial file under the final name

#[test]
fn m5_a_failed_check_leaves_no_file_and_a_good_one_leaves_only_the_final_name() {
    use botport_grok::files::{partial_name, save_new_checked, sha256_hex};
    let s = Scratch::new("m5");
    let e = save_new_checked(&s.0, "export.json", b"synthetic", |_, _| {
        Err("bad digest".into())
    })
    .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read_dir(&s.0).unwrap().count(),
        0,
        "nothing left behind"
    );

    // a crashed earlier run left a partial: it never shows under the final name and does not block
    std::fs::write(s.0.join(partial_name("export.json")), b"synth").unwrap();
    let (p, n, sha) = save_new_checked(&s.0, "export.json", b"synthetic", |n, sha| {
        assert_eq!((n, sha), (9, sha256_hex(b"synthetic").as_str()));
        Ok(())
    })
    .unwrap();
    assert_eq!(p, s.0.join("export.json"));
    assert_eq!((n, sha.len()), (9, 64));
    let names: Vec<String> = std::fs::read_dir(&s.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["export.json".to_string()]);
    // and the final name is never overwritten
    let (p2, _, _) = save_new_checked(&s.0, "export.json", b"other", |_, _| Ok(())).unwrap();
    assert_eq!(p2, s.0.join("export (1).json"));
    assert_eq!(std::fs::read(&p).unwrap(), b"synthetic");
}

#[test]
fn m5_a_download_with_a_bad_app_digest_leaves_no_partial() {
    const BOT2: &str = "00000000-0000-4000-8000-000000000002";
    let s = Scratch::new("m5dl");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_bad_app_digest();
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    assert_eq!(
        e.file_download(BOT2, &["export.json".into()], &s.0).exit,
        ExitCode::Other
    );
    assert_eq!(std::fs::read_dir(&s.0).unwrap().count(), 0);
}

// ------------------------------------------------------------------ M6: delete by name only when the name is unique

#[test]
fn m6_delete_refuses_a_name_two_files_share() {
    let c = clock();
    let a = format!("grok-bot/agents/{MAIN}/attachments/a/dup.json");
    let b = format!("grok-bot/agents/{MAIN}/attachments/b/dup.json");
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_file_at(MAIN, "dup.json", &a, b"{}".to_vec())
        .with_file_at(MAIN, "dup.json", &b, b"{\"n\":1}".to_vec())
        .with_file(MAIN, "solo.json", b"{}".to_vec());
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let o = e.file_delete(MAIN, &b, None, true);
    assert_eq!(o.exit, ExitCode::Refused, "{}", o.proof.line());
    assert!(
        o.proof.line().contains("2 files are named"),
        "{}",
        o.proof.line()
    );
    assert_eq!(e.files(MAIN).value.unwrap().len(), 3, "nothing deleted");
    let o = e.file_delete(MAIN, "solo.json", None, true);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(e.files(MAIN).value.unwrap().len(), 2);
}

// ------------------------------------------------------------------ L1: bounded port TTL

#[test]
fn l1_port_ttl_is_bounded_and_expiry_cannot_overflow() {
    use botport_grok::lifecycle::{PortFile, MAX_PORT_TTL_S};
    let mut p = Policy::default();
    assert!(p.set("port_ttl", "100000h").is_err());
    assert!(p.set("port_ttl", "1e30s").is_err());
    assert!(p.set("port_ttl", "infs").is_err());
    assert!(p.set("port_ttl", "24h").is_ok());
    assert_eq!(p.port_ttl_s, MAX_PORT_TTL_S);
    let pf = PortFile {
        port: 54321,
        pid: 1,
        ws_path: "/devtools/browser/synthetic".into(),
        opened_at_ms: 1_800_000_000_000,
        ttl_s: u64::MAX,
    };
    assert_eq!(pf.expires_ms(), u64::MAX);
}

// ------------------------------------------------------------------ L3, L4: private file modes

#[cfg(unix)]
fn mode(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn l3_downloads_and_fetches_are_0600() {
    const BOT2: &str = "00000000-0000-4000-8000-000000000002";
    let s = Scratch::new("l3");
    let mut e = eng(&[]);
    let o = e.file_download(BOT2, &["export.json".into()], &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(mode(&s.0.join("export.json")), 0o600);
    let (_f, p) = botport_grok::files::create_unique(&s.0, "x.txt").unwrap();
    assert_eq!(mode(&p), 0o600);
}

#[cfg(unix)]
#[test]
fn l4_existing_files_are_tightened_and_existing_folders_left_alone() {
    use botport_grok::util::{append_private, private_dir};
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("l4");
    std::fs::set_permissions(&s.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    private_dir(&s.0).unwrap();
    assert_eq!(
        mode(&s.0),
        0o755,
        "a folder botport did not create keeps its mode"
    );
    let new = s.0.join("home");
    private_dir(&new).unwrap();
    assert_eq!(mode(&new), 0o700);
    let log = new.join("grok-actions.jsonl");
    std::fs::write(&log, b"{}\n").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
    append_private(&log, "{}").unwrap();
    assert_eq!(mode(&log), 0o600);
}

// ------------------------------------------------------------------ L6: an unverifiable download says so

#[test]
fn l6_a_download_with_no_app_digest_and_no_size_is_only_a_self_check() {
    let s = Scratch::new("l6");
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_file(MAIN, "empty.txt", vec![])
        .without_app_digest();
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let o = e.file_download(MAIN, &["empty.txt".into()], &s.0);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(o.proof.route, "self-check");
    assert!(o.proof.line().contains("no sha256"), "{}", o.proof.line());
    // with a listed size it is still the files-list route
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_file(MAIN, "sized.txt", b"synthetic".to_vec())
        .without_app_digest();
    let mut e = engine_from(f, c, Policy::default(), Caller::Cli { tty: false });
    let o = e.file_download(MAIN, &["sized.txt".into()], &s.0);
    assert_eq!(o.proof.route, "files-list", "{}", o.proof.line());
}

// ------------------------------------------------------------------ L7: one botport process at a time on the page

#[test]
fn l7_the_page_lock_is_exclusive_and_engine_ops_take_it() {
    let s = Scratch::new("l7");
    let p = s.0.join("grok.lock");
    let held = botport_grok::util::lock_file(&p).unwrap();
    let other = std::fs::File::open(&p).unwrap();
    assert!(
        other.try_lock().is_err(),
        "a second holder is refused while the first holds it"
    );
    drop(held);
    assert!(other.try_lock().is_ok());
    drop(other);
    let mut e = eng(&[]);
    e.page_lock = Some(p.clone());
    assert_eq!(e.bots().exit, ExitCode::Done);
    // released after each operation
    let again = std::fs::File::open(&p).unwrap();
    assert!(again.try_lock().is_ok());
}

// ------------------------------------------------------------------ L8, L9

#[test]
fn l8_subscribe_is_not_on_the_ipc_allowlist() {
    use botport_grok::policy::floors;
    assert!(!floors::ipc_allowed("subscribe"));
    assert!(!floors::IPC_READ.contains(&"subscribe"));
}

#[test]
fn l9_a_bot_callable_server_with_an_allowlist_only_reaches_listed_bots() {
    let mut p = Policy::default();
    p.set("bots_scope", MAIN).unwrap();
    let c = clock();
    let f = FakeTransport::load(&fx(), c.clone())
        .unwrap()
        .with_turn("reply-ready");
    let mut e = engine_from(f, c, p, Caller::Mcp { bot_callable: true });
    let mut st = || json!({"exit": 0});
    let (v, err) = mcp::call(
        &mut e,
        &mut st,
        "grok_send",
        &json!({"bot": "00000000-0000-4000-8000-000000000002", "message": "hi"}),
        true,
    )
    .unwrap();
    assert!(err, "{v}");
    assert_eq!(v["exit"], json!(4), "{v}");
    let (v, err) = mcp::call(
        &mut e,
        &mut st,
        "grok_ask",
        &json!({"bot": MAIN, "message": "hi"}),
        true,
    )
    .unwrap();
    assert!(!err, "{v}");
}

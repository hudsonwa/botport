//! Presets x callers, the hard floors, the port lifecycle (fake host, virtual time), and the MCP mirror.
mod common;
use botport_grok::clock::{FakeClock, SharedClock};
use botport_grok::engine::ExitCode;
use botport_grok::lifecycle::{AppHost, Lifecycle, PortFile};
use botport_grok::mcp;
use botport_grok::policy::{floors, Action, Caller, Decision, Policy, Preset, Target};
use botport_grok::transport::fake::FakeTransport;
use common::*;
use serde_json::{json, Value};

const CLI: Caller = Caller::Cli { tty: false };
const TTY: Caller = Caller::Cli { tty: true };
const MCP: Caller = Caller::Mcp {
    bot_callable: false,
};
const BOTMCP: Caller = Caller::Mcp { bot_callable: true };

fn t<'a>(name: &'a str, main: bool) -> Target<'a> {
    Target {
        id: "00000000-0000-4000-8000-000000000002",
        name,
        main,
    }
}

fn refused(d: Decision, setting: &str) -> bool {
    matches!(d, Decision::Refuse { setting: s, .. } if s == setting)
}

// ---------------------------------------------------------------- floors

#[test]
fn floors_hold_under_every_preset_and_caller() {
    for p in [Preset::Careful, Preset::Balanced, Preset::Fast] {
        let pol = Policy::preset(p);
        for c in [CLI, TTY, MCP, BOTMCP] {
            assert!(
                refused(pol.decide(&c, &Action::AnswerApproval, None), "floor 2"),
                "{p:?} {c:?}"
            );
            for m in [
                "recordLocalToolApproval",
                "setLocalToolPermission",
                "setLocalToolActionGrant",
                "recordLocalToolStandingGrant",
                "setGrokBotPurchaseAutoApprove",
                "startCartCheckout",
                "listGrokBotVaultCredentials",
                "putGrokBotSecret",
                "installUpdate",
                "anythingElse",
            ] {
                assert!(
                    refused(pol.decide(&c, &Action::Ipc(m), None), "floor 6"),
                    "{m}"
                );
            }
            assert!(refused(
                pol.decide(&c, &Action::Bind("0.0.0.0:9222"), None),
                "floor 1"
            ));
            assert!(refused(
                pol.decide(&c, &Action::Bind("192.168.1.5:9222"), None),
                "floor 1"
            ));
            assert_eq!(
                pol.decide(&c, &Action::Bind("127.0.0.1:9222"), None),
                Decision::Allow
            );
            let tg = t("Ledger Bot", false);
            for typed in ["ledger bot", "Ledger", "led", ""] {
                let d = pol.decide(
                    &c,
                    &Action::Delete {
                        exact: typed,
                        confirm: Some(typed),
                        yes: true,
                    },
                    Some(&tg),
                );
                assert!(refused(d, "floor 4"), "{typed:?}");
            }
        }
    }
}

#[test]
fn ipc_allowlist_is_exactly_the_spec_list() {
    for m in floors::IPC_READ.iter().chain(floors::IPC_WRITE) {
        assert!(floors::ipc_allowed(m), "{m}");
    }
    assert!(!floors::ipc_allowed("getLocalToolPermissionX"));
    assert!(!floors::ipc_allowed("setLocalToolPermission"));
    assert!(!floors::ipc_allowed("updateGrokBotAgent"));
}

#[test]
fn bot_callable_mcp_never_gets_stop_delete_or_lifecycle() {
    for p in [Preset::Careful, Preset::Balanced, Preset::Fast] {
        let pol = Policy::preset(p);
        let tg = t("Ledger Bot", false);
        assert!(refused(
            pol.decide(&BOTMCP, &Action::Stop, Some(&tg)),
            "mcp"
        ));
        assert!(refused(pol.decide(&BOTMCP, &Action::Launch, None), "mcp"));
        assert!(refused(
            pol.decide(&BOTMCP, &Action::AutoLaunch, None),
            "mcp"
        ));
        let d = pol.decide(
            &BOTMCP,
            &Action::Delete {
                exact: "Ledger Bot",
                confirm: Some("Ledger Bot"),
                yes: true,
            },
            Some(&tg),
        );
        assert!(refused(d, "mcp"));
        if p != Preset::Careful {
            assert_eq!(
                pol.decide(&BOTMCP, &Action::Send, Some(&tg)),
                Decision::Allow
            );
        }
    }
    let names: Vec<String> = mcp::tools(true)
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(!names.iter().any(|n| n.contains("stop")
        || n.contains("delete")
        || n.contains("launch")
        || n.contains("restore")));
    assert!(mcp::tools(false).iter().any(|t| t["name"] == "grok_stop"));
}

// ---------------------------------------------------------------- presets x callers

#[test]
fn preset_table() {
    let fast = Policy::preset(Preset::Fast);
    let bal = Policy::preset(Preset::Balanced);
    let car = Policy::preset(Preset::Careful);
    assert_eq!(Policy::default(), fast, "default preset is fast");
    // automatic launch: fast yes, balanced asks on a TTY only, careful never
    assert_eq!(
        fast.decide(&CLI, &Action::AutoLaunch, None),
        Decision::Allow
    );
    assert!(matches!(
        bal.decide(&TTY, &Action::AutoLaunch, None),
        Decision::Ask(_)
    ));
    assert!(refused(
        bal.decide(&CLI, &Action::AutoLaunch, None),
        "debug_port"
    ));
    assert!(refused(
        bal.decide(&MCP, &Action::AutoLaunch, None),
        "debug_port"
    ));
    let mut rem = bal.clone();
    rem.launch_remembered = true;
    assert_eq!(rem.decide(&CLI, &Action::AutoLaunch, None), Decision::Allow);
    assert!(refused(
        car.decide(&TTY, &Action::AutoLaunch, None),
        "debug_port"
    ));
    // explicit launch allowed in every preset on the CLI
    for p in [&fast, &bal, &car] {
        assert_eq!(p.decide(&CLI, &Action::Launch, None), Decision::Allow);
    }
    assert!(refused(
        car.decide(&MCP, &Action::Launch, None),
        "mcp_lifecycle_tools"
    ));
    // destructive confirmation
    let tg = t("Ledger Bot", false);
    let del = |yes, confirm| Action::Delete {
        exact: "Ledger Bot",
        confirm,
        yes,
    };
    assert_eq!(
        fast.decide(&CLI, &del(false, None), Some(&tg)),
        Decision::Allow
    );
    assert_eq!(
        fast.decide(&MCP, &del(false, Some("Ledger Bot")), Some(&tg)),
        Decision::Allow
    );
    assert!(refused(
        bal.decide(&CLI, &del(false, None), Some(&tg)),
        "confirm_destructive"
    ));
    assert_eq!(
        bal.decide(&CLI, &del(true, None), Some(&tg)),
        Decision::Allow
    );
    assert!(refused(
        bal.decide(&MCP, &del(false, Some("ledger bot")), Some(&tg)),
        "floor 4"
    ));
    assert_eq!(
        bal.decide(&MCP, &del(false, Some("Ledger Bot")), Some(&tg)),
        Decision::Allow
    );
    let mut car_all = car.clone();
    car_all.set("bots_scope", "all").unwrap();
    assert!(matches!(
        car_all.decide(&TTY, &del(false, None), Some(&tg)),
        Decision::Ask(_)
    ));
    assert!(refused(
        car_all.decide(&MCP, &del(false, Some("Ledger Bot")), Some(&tg)),
        "confirm_destructive"
    ));
    // the main bot cannot be deleted
    assert!(refused(
        fast.decide(
            &CLI,
            &Action::Delete {
                exact: "Main Bot",
                confirm: None,
                yes: true
            },
            Some(&Target {
                id: MAIN,
                name: "Main Bot",
                main: true
            })
        ),
        "main bot"
    ));
    // secrets
    assert!(refused(
        fast.decide(&CLI, &Action::SecretValue { flag: false }, None),
        "secrets_to_grok"
    ));
    assert_eq!(
        fast.decide(&CLI, &Action::SecretValue { flag: true }, None),
        Decision::Allow
    );
    assert!(refused(
        bal.decide(&CLI, &Action::SecretValue { flag: true }, None),
        "secrets_to_grok"
    ));
    // careful: empty allowlist refuses every bot
    assert!(refused(
        car.decide(&CLI, &Action::Read, Some(&tg)),
        "bots_scope"
    ));
    // full action log in every preset
    assert!(fast.action_log && bal.action_log && car.action_log);
}

#[test]
fn policy_persists_0600_and_set_validates() {
    let s = Scratch::new("policy");
    assert_eq!(Policy::load(&s.0).unwrap(), Policy::default());
    let mut p = Policy::preset(Preset::Balanced);
    p.set("port_ttl", "45m").unwrap();
    p.set("transports.ax", "off").unwrap();
    p.set("bots_scope", "Ledger Bot, Scout Bot").unwrap();
    p.save(&s.0).unwrap();
    let q = Policy::load(&s.0).unwrap();
    assert_eq!(q, p);
    assert_eq!(q.port_ttl_s, 2700);
    assert!(!q.transports.ax);
    assert!(p.set("nope", "1").is_err());
    assert!(p.set("debug_port", "sometimes").is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(Policy::path(&s.0))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

// ---------------------------------------------------------------- lifecycle (fake host)

#[derive(Default)]
struct Host {
    installed: bool,
    pid: Option<u32>,
    dap: Option<(u16, String)>,
    listeners: Vec<String>,
    nonloop: bool,
    starts: Vec<bool>,
    armed: u32,
}

impl AppHost for Host {
    fn installed_version(&self) -> Option<String> {
        self.installed.then(|| "1.1.0".to_string())
    }
    fn main_pid(&self) -> Option<u32> {
        self.pid
    }
    fn quit(&mut self, _pid: u32) -> Result<(), String> {
        self.pid = None;
        self.listeners.clear();
        Ok(())
    }
    fn start(&mut self, debug: bool) -> Result<(), String> {
        self.starts.push(debug);
        self.pid = Some(4242 + self.starts.len() as u32);
        if debug {
            self.dap = Some((54321, "/devtools/browser/synthetic".into()));
            self.listeners = vec!["127.0.0.1:54321".into()];
            if self.nonloop {
                self.listeners.push("*:54322".into());
            }
        } else {
            self.listeners.clear();
        }
        Ok(())
    }
    fn devtools_active_port(&self) -> Option<(u16, String)> {
        self.dap.clone()
    }
    fn remove_devtools_active_port(&mut self) {
        self.dap = None;
    }
    fn tcp_listeners(&self, pid: u32) -> Vec<String> {
        if self.pid == Some(pid) {
            self.listeners.clone()
        } else {
            vec![]
        }
    }
    fn arm_guard(&mut self, _a: u64, _b: u64) -> Result<(), String> {
        self.armed += 1;
        Ok(())
    }
}

fn fclock() -> (std::sync::Arc<FakeClock>, SharedClock) {
    let c = FakeClock::new(1_800_000_000_000);
    let s: SharedClock = c.clone();
    (c, s)
}

#[test]
fn launch_opens_loopback_port_writes_0600_file_and_arms_the_guard() {
    let s = Scratch::new("launch");
    let mut h = Host {
        installed: true,
        pid: Some(100),
        ..Default::default()
    };
    let (_c, clk) = fclock();
    let mut lc = Lifecycle {
        host: &mut h,
        home: s.0.clone(),
        clock: clk,
    };
    let o = lc.launch(1800);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    let line = o.proof.line();
    assert!(
        line.starts_with("PASS: port open on 127.0.0.1 until "),
        "{line}"
    );
    assert!(line.contains("listener is Grok Bot pid 4243"));
    assert!(
        !line.contains("54321"),
        "the port number never reaches a proof (floor 5)"
    );
    assert_eq!(o.proof.route, "lsof");
    let pf = PortFile::load(&s.0).unwrap();
    assert_eq!((pf.port, pf.pid, pf.ttl_s), (54321, 4243, 1800));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(PortFile::path(&s.0))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(h.armed, 1);
    assert_eq!(h.starts, vec![true]);
}

#[test]
fn launch_with_a_non_loopback_listener_is_refused_and_restored() {
    let s = Scratch::new("nonloop");
    let mut h = Host {
        installed: true,
        pid: Some(100),
        nonloop: true,
        ..Default::default()
    };
    let (_c, clk) = fclock();
    let mut lc = Lifecycle {
        host: &mut h,
        home: s.0.clone(),
        clock: clk,
    };
    let o = lc.launch(600);
    assert_eq!(o.exit, ExitCode::Refused);
    assert!(o.proof.line().contains("floor 1"));
    assert!(PortFile::load(&s.0).is_none());
    assert_eq!(h.starts, vec![true, false]);
    assert!(h.listeners.is_empty());
}

#[test]
fn ttl_expiry_restores_and_status_reports_both_states() {
    let s = Scratch::new("ttl");
    let mut h = Host {
        installed: true,
        pid: Some(100),
        ..Default::default()
    };
    let (c, clk) = fclock();
    let pol = Policy::default();
    {
        let mut lc = Lifecycle {
            host: &mut h,
            home: s.0.clone(),
            clock: clk.clone(),
        };
        assert_eq!(lc.launch(600).exit, ExitCode::Done);
        let st = lc.status(&pol);
        assert!(
            st.proof.line().contains("debug port open until"),
            "{}",
            st.proof.line()
        );
        assert!(st.value.as_ref().unwrap().port_by_botport);
        assert!(!lc.enforce_ttl(), "not expired yet");
        c.advance(601_000);
        assert!(lc.enforce_ttl(), "expired: restored");
        let st = lc.status(&pol);
        assert_eq!(st.exit, ExitCode::Done);
        assert!(
            st.proof
                .line()
                .starts_with("PASS: Grok Bot 1.1.0 running; debug port closed; preset fast"),
            "{}",
            st.proof.line()
        );
    }
    assert!(PortFile::load(&s.0).is_none());
    assert_eq!(h.starts, vec![true, false]);
}

#[test]
fn guard_restores_at_expiry_unless_a_newer_launch_owns_the_port() {
    let s = Scratch::new("guard");
    let mut h = Host {
        installed: true,
        pid: Some(100),
        ..Default::default()
    };
    let (_c, clk) = fclock();
    let mut lc = Lifecycle {
        host: &mut h,
        home: s.0.clone(),
        clock: clk,
    };
    let pf = lc.launch(120).value.unwrap();
    assert!(
        !lc.guard(pf.opened_at_ms + 1),
        "a different launch: the guard leaves it"
    );
    assert!(lc.guard(pf.opened_at_ms), "same launch: restored at expiry");
    assert!(PortFile::load(&s.0).is_none());
}

#[test]
fn status_flags_a_port_botport_did_not_open() {
    let s = Scratch::new("foreign");
    let mut h = Host {
        installed: true,
        pid: Some(7),
        dap: Some((40000, "/x".into())),
        listeners: vec!["127.0.0.1:40000".into()],
        ..Default::default()
    };
    let (_c, clk) = fclock();
    let mut lc = Lifecycle {
        host: &mut h,
        home: s.0.clone(),
        clock: clk,
    };
    let o = lc.status(&Policy::default());
    assert!(
        o.proof.line().contains("OPEN (not opened by botport)"),
        "{}",
        o.proof.line()
    );
    let mut h2 = Host::default();
    let (_c, clk) = fclock();
    let mut lc = Lifecycle {
        host: &mut h2,
        home: s.0.clone(),
        clock: clk,
    };
    assert_eq!(lc.status(&Policy::default()).exit, ExitCode::Unavailable);
}

#[test]
fn restore_passes_on_lsof() {
    let s = Scratch::new("restore");
    let mut h = Host {
        installed: true,
        pid: Some(100),
        ..Default::default()
    };
    let (_c, clk) = fclock();
    let mut lc = Lifecycle {
        host: &mut h,
        home: s.0.clone(),
        clock: clk,
    };
    lc.launch(600);
    let o = lc.restore();
    assert_eq!(o.proof.line(), "PASS: port closed; no Grok TCP listener");
}

// ---------------------------------------------------------------- MCP

fn rpc(lines: &[Value], bot_callable: bool, turns: &[&str], policy: Policy) -> Vec<Value> {
    let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mut out = Vec::new();
    let turns: Vec<String> = turns.iter().map(|s| s.to_string()).collect();
    let c = Caller::Mcp { bot_callable };
    let mut make = || {
        let clk = clock();
        let mut f = FakeTransport::load(&fx(), clk.clone()).unwrap();
        for t in &turns {
            f = f.with_turn(t);
        }
        Ok(engine_from(f, clk, policy.clone(), c))
    };
    let mut status = || json!({"exit": 0, "proof": "PASS: status; self-check"});
    mcp::serve(
        &mut make,
        &mut status,
        input.as_bytes(),
        &mut out,
        bot_callable,
    )
    .unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
}

#[test]
fn mcp_initialize_list_and_ask() {
    let r = rpc(
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            call(3, "grok_ask", json!({"bot": "main", "message": "hi"})),
            call(4, "grok_bots", json!({})),
            json!({"jsonrpc": "2.0", "id": 5, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "nope"}),
        ],
        false,
        &["reply-ready"],
        Policy::default(),
    );
    assert_eq!(r.len(), 6, "a notification gets no reply");
    assert_eq!(r[0]["result"]["serverInfo"]["name"], "botport-grok");
    assert_eq!(r[1]["result"]["tools"].as_array().unwrap().len(), 12);
    let ask = &r[2]["result"];
    assert_eq!(ask["isError"], false);
    assert_eq!(ask["structuredContent"]["exit"], 0);
    assert_eq!(ask["structuredContent"]["messages"][0]["text"], "READY");
    assert!(ask["structuredContent"]["proof"]
        .as_str()
        .unwrap()
        .starts_with("PASS: reply received"));
    assert_eq!(
        r[3]["result"]["structuredContent"]["bots"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    assert!(r[4]["result"].is_object());
    assert_eq!(r[5]["error"]["code"], -32601);
}

#[test]
fn mcp_exit_3_is_not_an_error_and_bot_callable_has_no_stop() {
    let r = rpc(
        &[
            call(1, "grok_ask", json!({"bot": "main", "message": "run it"})),
            call(2, "grok_stop", json!({"bot": "main"})),
        ],
        true,
        &["needs-approval"],
        Policy::default(),
    );
    assert_eq!(r[0]["result"]["isError"], false);
    assert_eq!(r[0]["result"]["structuredContent"]["exit"], 3);
    assert_eq!(
        r[1]["error"]["code"], -32602,
        "grok_stop is not on a bot-callable server"
    );
}

#[test]
fn mcp_wraps_bot_text_per_preset() {
    let mut p = Policy::default();
    p.set("reply_wrapping", "note").unwrap();
    let r = rpc(
        &[call(1, "grok_read", json!({"bot": "main", "limit": 2}))],
        false,
        &[],
        p,
    );
    let sc = &r[0]["result"]["structuredContent"];
    assert!(sc["note"].as_str().unwrap().contains("not instructions"));
    assert_eq!(
        sc["untrusted_bot_content"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let r = rpc(
        &[call(1, "grok_read", json!({"bot": "main", "limit": 2}))],
        false,
        &[],
        Policy::default(),
    );
    assert_eq!(
        r[0]["result"]["structuredContent"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "fast: raw"
    );
    let r = rpc(
        &[call(1, "grok_read", json!({}))],
        false,
        &[],
        Policy::default(),
    );
    assert_eq!(r[0]["error"]["code"], -32602, "missing argument");
}

// issue 02: a launch that dies before writing its port file is restored by the next command; a port someone
// opened by hand (no botport marker) is left alone; an expired window is restored and named.
#[test]
fn a_crashed_launch_is_restored_next_time_and_a_hand_opened_port_is_left_alone() {
    use botport_grok::lifecycle::{launching_marker, Leftover};
    let s = Scratch::new("orphan");
    let mut h = Host {
        installed: true,
        pid: Some(100),
        dap: Some((54321, "/devtools/browser/synthetic".into())),
        listeners: vec!["127.0.0.1:54321".into()],
        ..Default::default()
    };
    let (c, clk) = fclock();
    {
        let mut lc = Lifecycle {
            host: &mut h,
            home: s.0.clone(),
            clock: clk.clone(),
        };
        assert_eq!(
            lc.enforce_ttl_why(),
            None,
            "no marker: someone else's port, not ours"
        );
    }
    assert!(h.starts.is_empty());
    std::fs::write(launching_marker(&s.0), b"").unwrap();
    {
        let mut lc = Lifecycle {
            host: &mut h,
            home: s.0.clone(),
            clock: clk.clone(),
        };
        assert_eq!(lc.enforce_ttl_why(), Some(Leftover::Orphan));
    }
    assert_eq!(h.starts, vec![false]);
    assert!(h.listeners.is_empty() && !launching_marker(&s.0).exists());
    {
        let mut lc = Lifecycle {
            host: &mut h,
            home: s.0.clone(),
            clock: clk.clone(),
        };
        assert_eq!(lc.launch(60).exit, ExitCode::Done);
        assert!(
            !launching_marker(&s.0).exists(),
            "a finished launch removes its marker"
        );
    }
    c.advance(61_000);
    let mut lc = Lifecycle {
        host: &mut h,
        home: s.0.clone(),
        clock: clk,
    };
    assert_eq!(lc.enforce_ttl_why(), Some(Leftover::Expired));
    assert!(PortFile::load(&s.0).is_none());
}

// review L1: a launch never opens the port for more than 24 h, whatever the policy file says
#[test]
fn launch_clamps_the_port_ttl_to_24h() {
    let s = Scratch::new("ttlclamp");
    let mut h = Host {
        installed: true,
        pid: Some(100),
        ..Default::default()
    };
    let (_c, clk) = fclock();
    let mut lc = Lifecycle {
        host: &mut h,
        home: s.0.clone(),
        clock: clk,
    };
    let o = lc.launch(u64::MAX / 2);
    assert_eq!(o.exit, ExitCode::Done, "{}", o.proof.line());
    assert_eq!(
        PortFile::load(&s.0).unwrap().ttl_s,
        botport_grok::lifecycle::MAX_PORT_TTL_S
    );
}

// review L7: while another process is between `start` and saving its port file (it holds grok.lock), a TTL check
// waits for it instead of restoring the app under it as an "orphan"
#[test]
fn a_launch_in_progress_elsewhere_is_not_restored_as_an_orphan() {
    use botport_grok::lifecycle::{home_lock, launching_marker};
    let s = Scratch::new("lock");
    std::fs::create_dir_all(&s.0).unwrap();
    std::fs::write(launching_marker(&s.0), b"").unwrap();
    let held = botport_grok::util::lock_file(&home_lock(&s.0)).unwrap();
    let home = s.0.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut h = Host {
            installed: true,
            pid: Some(4243),
            dap: Some((54321, "/devtools/browser/synthetic".into())),
            listeners: vec!["127.0.0.1:54321".into()],
            ..Default::default()
        };
        let (_c, clk) = fclock();
        let mut lc = Lifecycle {
            host: &mut h,
            home,
            clock: clk,
        };
        let r = lc.enforce_ttl_why();
        tx.send(()).unwrap();
        (r, h.starts)
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(rx.try_recv().is_err(), "the check waits for the lock");
    // the other "launch" finishes: port file saved, marker removed, lock released
    PortFile {
        port: 54321,
        pid: 4243,
        ws_path: "/devtools/browser/synthetic".into(),
        opened_at_ms: 1_800_000_000_000,
        ttl_s: 600,
    }
    .save(&s.0)
    .unwrap();
    std::fs::remove_file(launching_marker(&s.0)).unwrap();
    drop(held);
    let (r, starts) = t.join().unwrap();
    assert_eq!(r, None);
    assert!(starts.is_empty(), "nothing restarted");
}

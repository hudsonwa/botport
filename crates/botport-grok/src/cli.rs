//! `botport grok <command>`: argument parsing, transport choice, human and JSON output, exit codes.
use crate::clock::{FakeClock, SharedClock, SystemClock};
use crate::engine::{Engine, ExitCode, Outcome, Proof, WaitOpts};
use crate::lifecycle::{AppHost, Lifecycle, MacHost, PortFile};
use crate::log::ActionLog;
use crate::mcp::{outcome_json, wrap};
use crate::model::*;
use crate::policy::{Action, Caller, Decision, Policy, Preset};
use crate::transport::cdp::CdpTransport;
use crate::transport::fake::FakeTransport;
use crate::transport::Transport;
use crate::util::botport_home;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

pub const COMMANDS: &[&str] = &[
    "status",
    "launch",
    "restore",
    "bots",
    "ask",
    "send",
    "wait",
    "read",
    "stop",
    "watch",
    "policy",
    "mcp",
    "canary",
    "routines",
    "usage",
    "files",
    "_ttl-guard",
];

pub const USAGE: &str = "botport grok <command> [--json] [--quiet] [--transport auto|debug-port|fake]
  status                                  app found, version, port open/closed, preset
  launch [--port-ttl 60m]                 restart Grok Bot with the loopback debug port (random port, 0600 port file)
  launch --for 10m [--when-idle 5]        a short window for scheduled work: reuses an open botport port, else only
                                          restarts when the Mac has been idle 5 min (exit 2 if not); restored at the end
  restore                                 restart Grok Bot with no port; delete the port file
  bots                                    list bots with state (idle | working | needs_input)
  send <bot> <message...>                 send a message; prints its key
  ask  <bot> <message...> [--timeout 300] [--settle 8]   send, then wait for the reply
  wait <bot> [--since KEY] [--timeout 1800] [--settle 8]
  read <bot> [--limit N] [--since KEY]
  stop <bot> [--steer TEXT]               steer message, then verify idle (1.1.0 has no native stop)
  watch [bot] [--for 10m] [--extend]      one line per state change or new message
  policy show | preset careful|balanced|fast | set KEY VALUE
  mcp [--bot-callable]                    stdio MCP server over the same functions
  canary                                  after an app update: port still opens, read-only probe per command, report
  routines <bot> [list]                   the bot's routines (schedule, enabled, next/last run)
  routines audit [--used-by NAME]...      every bot's routines: runs/week, ~turns/week, last run; flags a routine
                                          no mirror pair (or NAME) mentions. Lists only: 0 bot turns
  routines <bot> enable|disable <routine> direct toggle; PASS when a fresh list agrees
  routines <bot> add --name N --schedule \"0 9 * * 1\" [--tz UTC] --prompt P [--disabled]   asks the bot
  routines <bot> remove <routine> [--yes] exact id, slug or name only; the app's menu, else asks the bot
  routines <bot> run <routine>            asks the bot to run it now; PASS when a new run is listed
  usage                                   weekly Grok usage (checked before every send/ask/routine run)
  files <bot> [list]                      the bot's files
  files <bot> download [NAME...] [--out DIR]   save (all when no NAME); never overwrites; sha256 checked
  files <bot> fetch [NAME|KEY] [--out DIR]    save a file the bot sent in chat (newest when no NAME); never overwrites
  files <bot> upload PATH...              25 MB each; PASS when the list shows each with the same size
  files <bot> delete NAME [--yes]         exact name only
exit: 0 done, 1 other failure, 2 timeout (still working), 3 waiting for you, 4 refused by policy, 5 app/transport unavailable
fake transport (offline, replays fixtures): --transport fake --fixtures DIR [--turn NAME]... [--scenario idle|awaiting|routine-approval]
common: --trust-version  --no-log  --yes";

const VALUED: &[&str] = &[
    "transport",
    "timeout",
    "settle",
    "since",
    "limit",
    "steer",
    "port-ttl",
    "for",
    "fixtures",
    "turn",
    "scenario",
    "opened-at",
    "file",
    "name",
    "schedule",
    "tz",
    "prompt",
    "out",
    "when-idle",
    "used-by",
];
const FLAGS: &[&str] = &[
    "json",
    "quiet",
    "trust-version",
    "no-log",
    "extend",
    "bot-callable",
    "yes",
    "help",
    "write",
    "disabled",
];

#[derive(Default, Debug)]
pub struct Args {
    pub pos: Vec<String>,
    pub vals: Vec<(String, String)>,
    pub flags: Vec<String>,
}

impl Args {
    pub fn parse(raw: &[String]) -> Result<Self, String> {
        let mut a = Args::default();
        let mut i = 0;
        let mut rest = false;
        while i < raw.len() {
            let s = &raw[i];
            if rest || !s.starts_with("--") {
                a.pos.push(s.clone());
            } else if s == "--" {
                rest = true;
            } else {
                let (k, inline) = match s[2..].split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (s[2..].to_string(), None),
                };
                if VALUED.contains(&k.as_str()) {
                    let v = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            raw.get(i)
                                .cloned()
                                .ok_or_else(|| format!("--{k} needs a value"))?
                        }
                    };
                    a.vals.push((k, v));
                } else if FLAGS.contains(&k.as_str()) {
                    a.flags.push(k);
                } else {
                    return Err(format!("unknown option --{k}"));
                }
            }
            i += 1;
        }
        Ok(a)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.vals
            .iter()
            .rev()
            .find(|(x, _)| x == k)
            .map(|(_, v)| v.as_str())
    }
    pub fn all(&self, k: &str) -> Vec<&str> {
        self.vals
            .iter()
            .filter(|(x, _)| x == k)
            .map(|(_, v)| v.as_str())
            .collect()
    }
    pub fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|f| f == k)
    }
    fn num(&self, k: &str, d: f64) -> Result<f64, String> {
        match self.get(k) {
            None => Ok(d),
            Some(v) => crate::util::parse_duration_s(v)
                .map(|s| s as f64)
                .ok_or_else(|| format!("--{k}: not a duration: {v:?}")),
        }
    }
}

struct Out {
    json: bool,
    quiet: bool,
}

impl Out {
    /// Print the result: human lines (then the proof line) or one JSON object (proof also on stderr).
    fn emit<T: Serialize>(
        &self,
        o: &Outcome<T>,
        j: Value,
        human: impl FnOnce(&T) -> Vec<String>,
    ) -> u8 {
        if !self.quiet {
            for w in &o.warnings {
                eprintln!("warning: {w}");
            }
        }
        if self.json {
            println!("{j}");
            eprintln!("{}", o.proof.line());
        } else {
            if let Some(v) = &o.value {
                for l in human(v) {
                    println!("{l}");
                }
            }
            if let Some(h) = &o.hint {
                if !o.proof.pass {
                    println!("hint: {h}");
                }
            }
            println!("{}", o.proof.line());
        }
        o.exit.code()
    }
}

fn host_status(
    host: &mut dyn AppHost,
    clock: SharedClock,
    policy: &Policy,
) -> Outcome<crate::lifecycle::Status> {
    let mut lc = Lifecycle {
        host,
        home: botport_home(),
        clock,
    };
    lc.enforce_ttl();
    let home = lc.home.clone();
    let mut o = lc.status(policy);
    // the local-tool permission, read only, only over a port botport opened and that is still open (no launch)
    if let Some(st) = o
        .value
        .as_mut()
        .filter(|s| s.port_by_botport && s.port_open)
    {
        if let Some(pf) = PortFile::load(&home) {
            if let Ok(mut t) = CdpTransport::connect(&pf, &*host) {
                st.local_tool_permission = t.local_tool_permission().ok();
            }
        }
    }
    o
}

fn caller() -> Caller {
    Caller::Cli {
        tty: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
    }
}

fn tty_asker() -> crate::engine::Asker {
    Box::new(|q: &str| {
        if !std::io::stdin().is_terminal() {
            return false;
        }
        eprint!("{q} [y/N] ");
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).is_ok() && matches!(s.trim(), "y" | "Y" | "yes")
    })
}

/// Make the engine for a command: the fake (offline) or the debug port (opening it if the policy allows).
#[allow(clippy::result_large_err)] // built once per command; the error is printed and the process exits
pub fn build_engine(a: &Args, policy: &Policy, caller: Caller) -> Result<Engine, Outcome<()>> {
    let unavailable = |why: &str, hint: &str| {
        Outcome::fail(
            ExitCode::Unavailable,
            "transport",
            why.to_string(),
            Some(hint.to_string()),
        )
    };
    let fake_dir = a
        .get("fixtures")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("BOTPORT_GROK_FAKE").map(PathBuf::from));
    let tname = a
        .get("transport")
        .unwrap_or(if fake_dir.is_some() { "fake" } else { "auto" });
    let log = if policy.action_log && !a.has("no-log") {
        ActionLog::new(Some(botport_home().join("grok-actions.jsonl")))
    } else {
        ActionLog::off()
    };
    let mut e = match tname {
        "fake" => {
            let dir = fake_dir.ok_or_else(|| {
                unavailable(
                    "the fake transport needs --fixtures DIR",
                    "--fixtures tests/fixtures/app-1.1.0",
                )
            })?;
            let clock = FakeClock::new(crate::util::wall_ms());
            let shared: SharedClock = clock.clone();
            let mut f = match a.get("scenario").unwrap_or("idle") {
                "awaiting" => FakeTransport::load_awaiting(&dir, shared.clone()),
                "routine-approval" => FakeTransport::load_routine_awaiting(&dir, shared.clone()),
                _ => FakeTransport::load(&dir, shared.clone()),
            }
            .map_err(|e| unavailable(&e.to_string(), "check --fixtures"))?;
            for t in a.all("turn") {
                f = f.with_turn(t);
            }
            Engine::new(
                vec![Box::new(f) as Box<dyn Transport>],
                policy.clone(),
                shared,
                caller,
            )
        }
        "auto" | "debug-port" => {
            if !policy.transports.debug_port {
                return Err(unavailable(
                    "the debug-port transport is off",
                    "botport grok policy set transports.debug-port on",
                ));
            }
            let clock: SharedClock = Arc::new(SystemClock);
            let mut host = MacHost::default();
            let home = botport_home();
            let mut lc = Lifecycle {
                host: &mut host,
                home: home.clone(),
                clock: clock.clone(),
            };
            lc.enforce_ttl();
            let mut launched = false;
            let pf = match PortFile::load(&home).filter(|p| lc.host.main_pid() == Some(p.pid)) {
                Some(p) => p,
                None => {
                    if lc.host.installed_version().is_none() {
                        return Err(unavailable(
                            "Grok Bot is not installed",
                            "install the Grok Bot desktop app",
                        ));
                    }
                    let mut p2 = policy.clone();
                    match policy.decide(&caller, &Action::AutoLaunch, None) {
                        Decision::Allow => {}
                        Decision::Ask(q) if tty_asker()(&q) => {
                            p2.launch_remembered = true;
                            let _ = p2.save(&home);
                        }
                        Decision::Ask(_) => {
                            return Err(Outcome::fail(
                                ExitCode::Refused,
                                "launch",
                                "not confirmed",
                                Some("botport grok launch".into()),
                            ))
                        }
                        Decision::Refuse { setting, hint } => {
                            return Err(Outcome::fail(
                                ExitCode::Unavailable,
                                "transport",
                                format!("debug port closed; automatic launch refused by {setting}"),
                                Some(hint),
                            ))
                        }
                    }
                    let o = lc.launch(p2.port_ttl_s);
                    launched = true;
                    match o.value {
                        Some(p) => p,
                        None => {
                            return Err(Outcome {
                                value: None,
                                exit: o.exit,
                                proof: o.proof,
                                hint: o.hint,
                                warnings: o.warnings,
                            })
                        }
                    }
                }
            };
            let t = CdpTransport::connect(&pf, &host)
                .and_then(|mut t| {
                    if launched {
                        // auto-launch: the first read must not race the app's first render
                        t.wait_rendered(&clock, 20_000)?;
                    }
                    Ok(t)
                })
                .map_err(|e| {
                    let exit = if matches!(e, crate::TransportError::Floor(_)) {
                        ExitCode::Refused
                    } else {
                        ExitCode::Unavailable
                    };
                    Outcome::fail(
                        exit,
                        "transport",
                        e.to_string(),
                        Some("botport grok restore; botport grok launch".to_string()),
                    )
                })?;
            let mut e = Engine::new(
                vec![Box::new(t) as Box<dyn Transport>],
                policy.clone(),
                clock,
                caller,
            );
            if !a.has("extend") {
                e.deadline_ms = Some(pf.expires_ms());
            }
            e.page_lock = Some(crate::lifecycle::home_lock(&home));
            e
        }
        "ax" | "bot-shell" | "webhook" => {
            return Err(unavailable(
                &format!("the {tname} transport is not built yet (P1 has debug-port only)"),
                "botport grok launch",
            ))
        }
        other => {
            return Err(unavailable(
                &format!("unknown transport {other:?}"),
                "--transport auto|debug-port|fake",
            ))
        }
    };
    e.trust_version = a.has("trust-version");
    e.canary_dir = Some(botport_home().join("canary"));
    e.log = log;
    e.asker = tty_asker();
    Ok(e)
}

/// H1: an MCP engine built on a port file may keep serving only while that port file is still there, names the
/// running app's main pid, and (without `--extend`) has the expiry the engine was built with.
pub fn port_file_matches(
    pf: Option<&PortFile>,
    main_pid: Option<u32>,
    deadline_ms: Option<u64>,
) -> bool {
    pf.is_some_and(|p| Some(p.pid) == main_pid && deadline_ms.is_none_or(|d| d == p.expires_ms()))
}

fn fail_line(out: &Out, o: Outcome<()>) -> u8 {
    let j = outcome_json(&o, None);
    out.emit(&o, j, |_| vec![])
}

fn bot_line(b: &Bot) -> String {
    let mut s = format!("{}  {}", b.name, b.state.as_str());
    if b.main {
        s.push_str("  [main]");
    }
    if let Some(n) = &b.needs_input {
        s.push_str(&format!("  waiting for you: {}", n.reason));
    } else if let Some(a) = &b.activity {
        s.push_str(&format!("  {a}"));
    }
    s.push_str(&format!("  {}", b.id));
    s
}

fn msg_line(m: &Message) -> String {
    let who = match m.role {
        Role::User => "you",
        Role::Bot => "bot",
        Role::Event => "event",
    };
    format!("[{}] {who}: {}", m.key, m.text)
}

/// Entry point for `botport grok …` (args after `grok`). Returns the exit code.
pub fn main(raw: &[String]) -> u8 {
    let a = match Args::parse(raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("botport grok: {e}\n{USAGE}");
            println!("FAIL: arguments; {e}");
            return 1;
        }
    };
    let out = Out {
        json: a.has("json"),
        quiet: a.has("quiet"),
    };
    let cmd = a.pos.first().cloned().unwrap_or_default();
    if a.has("help") || cmd.is_empty() || cmd == "help" {
        println!("{USAGE}");
        return if cmd.is_empty() && !a.has("help") {
            1
        } else {
            0
        };
    }
    let home = botport_home();
    let mut policy = match Policy::load(&home) {
        Ok(p) => p,
        Err(e) => return fail_line(&out, Outcome::fail(ExitCode::Other, "policy", e, None)),
    };
    let pos = |i: usize| a.pos.get(i).map(String::as_str).unwrap_or("");
    let rest = |i: usize| a.pos.get(i..).map(|v| v.join(" ")).unwrap_or_default();
    let fake = a.get("transport") == Some("fake")
        || a.get("fixtures").is_some()
        || std::env::var_os("BOTPORT_GROK_FAKE").is_some();

    match cmd.as_str() {
        "status" => {
            let o = if fake {
                match build_engine(&a, &policy, caller()) {
                    Ok(mut e) => fake_status(&mut e, &policy),
                    Err(o) => return fail_line(&out, o),
                }
            } else {
                host_status(&mut MacHost::default(), Arc::new(SystemClock), &policy)
            };
            let j = outcome_json(&o, None);
            out.emit(&o, j, |s| {
                let port = match (s.port_open, s.port_expires_at.as_deref()) {
                    (true, Some(t)) => format!("open until {}", crate::util::local_from_rfc3339(t)),
                    (true, None) => "open".to_string(),
                    _ => "closed".to_string(),
                };
                vec![format!(
                    "Grok Bot {} | running: {} | debug port: {} | preset: {} | tested version: {}",
                    s.version.as_deref().unwrap_or("-"),
                    s.running,
                    port,
                    s.preset,
                    s.tested
                )]
            })
        }
        "launch" | "restore" => {
            if fake {
                return fail_line(
                    &out,
                    Outcome::fail(
                        ExitCode::Other,
                        &cmd,
                        "launch/restore act on the real app; not with the fake transport",
                        None,
                    ),
                );
            }
            if let Decision::Refuse { setting, hint } =
                policy.decide(&caller(), &Action::Launch, None)
            {
                return fail_line(
                    &out,
                    Outcome::fail(
                        ExitCode::Refused,
                        &cmd,
                        format!("refused by {setting}"),
                        Some(hint),
                    ),
                );
            }
            let mut host = MacHost::default();
            let mut lc = Lifecycle {
                host: &mut host,
                home,
                clock: Arc::new(SystemClock),
            };
            if cmd == "launch" {
                if let Some(o) = launch_window_gate(&a, &mut lc) {
                    return o(&out);
                }
                let ttl = match a.get("for").or(a.get("port-ttl")) {
                    Some(v) => match crate::util::parse_duration_s(v)
                        .filter(|s| *s > 0 && *s <= crate::lifecycle::MAX_PORT_TTL_S)
                    {
                        Some(s) => s,
                        None => {
                            return fail_line(
                                &out,
                                Outcome::fail(
                                    ExitCode::Other,
                                    "launch",
                                    format!("--port-ttl: bad duration {v:?}"),
                                    None,
                                ),
                            )
                        }
                    },
                    None => policy.port_ttl_s,
                };
                let mut o = lc.launch(ttl);
                if let Some(pf) = &o.value {
                    // PASS means usable: the next command (often another process) must not race the first render
                    let clock: SharedClock = Arc::new(SystemClock);
                    let window = a.get("for").is_some();
                    let ready = CdpTransport::connect(pf, &*lc.host).map(|mut t| {
                        let r = t.wait_rendered(&clock, 20_000);
                        (r, t.signed_out(), t.version().to_string())
                    });
                    let fail = match ready {
                        Ok((_, true, _)) => Some((
                            ExitCode::Unavailable,
                            "Grok Bot is signed out (its sign-in screen is showing); sign in, then run it again".to_string(),
                        )),
                        Ok((_, _, v)) if window && !crate::version::is_tested(&v) => Some((
                            ExitCode::Refused,
                            format!("Grok Bot {v} is not a tested version (unknown_version_writes); run botport grok canary"),
                        )),
                        Ok((Err(e), _, _)) | Err(e) => {
                            o.warnings
                                .push(format!("the app's window did not render within 20 s: {e}"));
                            None
                        }
                        _ => None,
                    };
                    if let Some((exit, why)) = fail {
                        let _ = lc.restore();
                        return fail_line(
                            &out,
                            Outcome::fail(exit, "launch", format!("{why}; restored"), None),
                        );
                    }
                }
                // never print the port number or websocket path (floor 5)
                let j = outcome_json(&Outcome { value: o.value.as_ref().map(|p| json!({"pid": p.pid, "opened_at": crate::util::rfc3339(p.opened_at_ms), "expires_at": crate::util::rfc3339(p.expires_ms())})), exit: o.exit, proof: o.proof.clone(), hint: o.hint.clone(), warnings: o.warnings.clone() }, None);
                out.emit(&o, j, |p| {
                    vec![format!(
                        "debug port open until {}",
                        crate::util::local(p.expires_ms())
                    )]
                })
            } else {
                let o = lc.restore();
                let j = outcome_json(&o, None);
                out.emit(&o, j, |_| vec![])
            }
        }
        "_ttl-guard" => {
            let Some(at) = a.get("opened-at").and_then(|v| v.parse().ok()) else {
                return 1;
            };
            let mut host = MacHost::default();
            let mut lc = Lifecycle {
                host: &mut host,
                home,
                clock: Arc::new(SystemClock),
            };
            u8::from(!lc.guard(at))
        }
        "policy" => cmd_policy(&a, &out, &mut policy, &home),
        "canary" => cmd_canary(&a, &out, &policy, &home, fake),
        "mcp" => {
            let bot_callable = a.has("bot-callable");
            let c = Caller::Mcp { bot_callable };
            let pol = policy.clone();
            let mut make = || {
                build_engine(&a, &pol, c)
                    .map(|mut e| {
                        e.asker = Box::new(|_| false);
                        e
                    })
                    .map_err(|o| outcome_json(&o, None))
            };
            let pol2 = policy.clone();
            let mut status = || {
                let o = if fake {
                    match build_engine(&a, &pol2, c) {
                        Ok(mut e) => fake_status(&mut e, &pol2),
                        Err(o) => return outcome_json(&o, None),
                    }
                } else {
                    host_status(&mut MacHost::default(), Arc::new(SystemClock), &pol2)
                };
                outcome_json(&o, None)
            };
            // H1: before each tool call, floor 5 again (restore an expired port) and drop an engine whose port
            // file is gone or belongs to another app process; the next call rebuilds it under the launch policy.
            let mut fresh = |e: &Engine| {
                if fake {
                    return true;
                }
                let mut host = MacHost::default();
                let home = botport_home();
                let mut lc = Lifecycle {
                    host: &mut host,
                    home: home.clone(),
                    clock: Arc::new(SystemClock),
                };
                lc.enforce_ttl();
                port_file_matches(
                    PortFile::load(&home).as_ref(),
                    lc.host.main_pid(),
                    e.deadline_ms,
                )
            };
            let stdin = std::io::stdin();
            match crate::mcp::serve_checked(
                &mut make,
                &mut status,
                &mut fresh,
                stdin.lock(),
                std::io::stdout(),
                bot_callable,
            ) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        }
        "routines" | "usage" | "files" => cmd_p3(&a, &out, &policy),
        "bots" | "send" | "ask" | "wait" | "read" | "stop" | "watch" => {
            if matches!(cmd.as_str(), "send" | "ask")
                && (pos(1).is_empty() || rest(2).trim().is_empty())
            {
                return fail_line(
                    &out,
                    Outcome::fail(
                        ExitCode::Other,
                        &cmd,
                        "usage: botport grok ask|send <bot> <message...>",
                        None,
                    ),
                );
            }
            if matches!(cmd.as_str(), "wait" | "read" | "stop") && pos(1).is_empty() {
                return fail_line(
                    &out,
                    Outcome::fail(
                        ExitCode::Other,
                        &cmd,
                        format!("usage: botport grok {cmd} <bot>"),
                        None,
                    ),
                );
            }
            let files: Vec<PathBuf> = a.all("file").into_iter().map(PathBuf::from).collect();
            let mut e = match build_engine(&a, &policy, caller()) {
                Ok(e) => e,
                Err(o) => return fail_line(&out, o),
            };
            let wopts = |t: f64| -> Result<WaitOpts, String> {
                Ok(WaitOpts {
                    timeout_s: a.num("timeout", t)?,
                    settle_s: a.num("settle", 8.0)?,
                })
            };
            match cmd.as_str() {
                "bots" => {
                    let o = e.bots();
                    let j = outcome_json(&o, Some("bots"));
                    out.emit(&o, j, |v| v.iter().map(bot_line).collect())
                }
                "send" => {
                    let o = e.send(pos(1), &rest(2), &files);
                    let j = outcome_json(&o, None);
                    out.emit(&o, j, |v| vec![format!("sent {} to {}", v.key, v.bot.name)])
                }
                "ask" | "wait" => {
                    let w = match wopts(if cmd == "ask" { 300.0 } else { 1800.0 }) {
                        Ok(w) => w,
                        Err(m) => {
                            return fail_line(&out, Outcome::fail(ExitCode::Other, &cmd, m, None))
                        }
                    };
                    let o = if cmd == "ask" {
                        e.ask(pos(1), &rest(2), &files, &w)
                    } else {
                        e.wait(pos(1), a.get("since"), &w)
                    };
                    let j = wrap(&e.policy, outcome_json(&o, None));
                    out.emit(&o, j, |v| v.messages.iter().map(msg_line).collect())
                }
                "read" => {
                    let limit = a.get("limit").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let o = e.read(pos(1), a.get("since"), limit);
                    let j = wrap(&e.policy, outcome_json(&o, Some("messages")));
                    out.emit(&o, j, |v| v.iter().map(msg_line).collect())
                }
                "stop" => {
                    let o = e.stop(pos(1), a.get("steer"));
                    let j = outcome_json(&o, None);
                    out.emit(&o, j, |v| {
                        vec![format!("{}: {}", v.bot.name, v.state.as_str())]
                    })
                }
                _ => {
                    let for_s = match a.get("for") {
                        Some(v) => match crate::util::parse_duration_s(v) {
                            Some(s) => Some(s as f64),
                            None => {
                                return fail_line(
                                    &out,
                                    Outcome::fail(
                                        ExitCode::Other,
                                        "watch",
                                        format!("--for: bad duration {v:?}"),
                                        None,
                                    ),
                                )
                            }
                        },
                        None => None,
                    };
                    if for_s.is_none() && fake {
                        return fail_line(
                            &out,
                            Outcome::fail(
                                ExitCode::Other,
                                "watch",
                                "the fake transport needs --for DUR",
                                None,
                            ),
                        );
                    }
                    sigint::install(e.interrupted.clone());
                    let json = out.json;
                    let mut sink = |ev: &WatchEvent| {
                        if json {
                            println!("{}", serde_json::to_string(ev).unwrap_or_default());
                        } else {
                            let m = ev
                                .message
                                .as_ref()
                                .map(|m| format!(": {}", m.text))
                                .unwrap_or_default();
                            let at = crate::util::local_from_rfc3339(&ev.at);
                            println!("{at} {} {}{m}", ev.bot, ev.kind);
                        }
                    };
                    let sel = Some(pos(1)).filter(|s| !s.is_empty());
                    let o = e.watch(sel, for_s, &mut sink);
                    if json {
                        // JSON lines: the summary is the last line
                        println!(
                            "{}",
                            json!({"type": "summary", "summary": o.value, "exit": o.exit.code(), "proof": o.proof.line()})
                        );
                        eprintln!("{}", o.proof.line());
                        o.exit.code()
                    } else {
                        out.emit(&o, Value::Null, |_| vec![])
                    }
                }
            }
        }
        other => fail_line(
            &out,
            Outcome::fail(
                ExitCode::Other,
                "command",
                format!("unknown grok command {other:?}"),
                Some("botport grok --help".into()),
            ),
        ),
    }
}

fn routine_line(r: &Routine) -> String {
    format!(
        "{}  {}  {}{}  next {}  last {}{}  {}",
        r.name,
        if r.enabled { "enabled" } else { "disabled" },
        r.schedule,
        r.tz.as_deref()
            .map(|t| format!(" ({t})"))
            .unwrap_or_default(),
        r.next_run
            .as_deref()
            .map(crate::util::local_from_rfc3339)
            .unwrap_or_else(|| "-".into()),
        r.last_run
            .as_deref()
            .map(crate::util::local_from_rfc3339)
            .unwrap_or_else(|| "-".into()),
        if r.running { "  [running]" } else { "" },
        r.slug
    )
}

fn file_line(f: &FileEntry) -> String {
    format!(
        "{}  {} bytes  {}  {}",
        f.name,
        f.bytes,
        if f.content_type.is_empty() {
            "-"
        } else {
            &f.content_type
        },
        f.updated_at
            .as_deref()
            .map(crate::util::local_from_rfc3339)
            .unwrap_or_else(|| "-".into())
    )
}

/// `launch --for DUR [--when-idle MIN]` (issue 02): reuse a live botport port (no restart) when it lasts long
/// enough; otherwise, with `--when-idle`, restart only after MIN minutes without keyboard/mouse input (exit 2 when
/// not idle, so a scheduler retries later). An earlier window that died is restored first and said so.
type Emit = Box<dyn FnOnce(&Out) -> u8>;
fn launch_window_gate(a: &Args, lc: &mut Lifecycle) -> Option<Emit> {
    let dur = a.get("for")?;
    let Some(for_s) = crate::util::parse_duration_s(dur)
        .filter(|s| *s > 0 && *s <= crate::lifecycle::MAX_PORT_TTL_S)
    else {
        let why = format!("--for: bad duration {dur:?}");
        return Some(Box::new(move |out| {
            fail_line(out, Outcome::fail(ExitCode::Other, "launch", why, None))
        }));
    };
    let mut warnings = vec![];
    match lc.enforce_ttl_why() {
        Some(crate::lifecycle::Leftover::Expired) => warnings
            .push("an earlier port window had outlived its TTL (its job or guard died); restored it first".into()),
        Some(crate::lifecycle::Leftover::Orphan) => warnings
            .push("an earlier launch died before writing its port file; restored the app first".into()),
        None => {}
    }
    let now = crate::util::wall_ms();
    if let Some(pf) = PortFile::load(&lc.home).filter(|p| lc.host.main_pid() == Some(p.pid)) {
        if pf.expires_ms() >= now + for_s * 1000 {
            let o = Outcome {
                value: Some(
                    json!({"pid": pf.pid, "opened_at": crate::util::rfc3339(pf.opened_at_ms), "expires_at": crate::util::rfc3339(pf.expires_ms()), "reused": true}),
                ),
                exit: ExitCode::Done,
                proof: Proof::pass(
                    format!(
                        "port already open until {}",
                        crate::util::hhmm_local(pf.expires_ms())
                    ),
                    "reused; no restart",
                    "port-file",
                ),
                hint: None,
                warnings,
            };
            return Some(Box::new(move |out| {
                let j = outcome_json(&o, None);
                out.emit(&o, j, |_| vec![])
            }));
        }
    }
    if let Some(min) = a.get("when-idle") {
        let Some(min) = min.parse::<u64>().ok() else {
            return Some(Box::new(|out| {
                fail_line(
                    out,
                    Outcome::fail(ExitCode::Other, "launch", "--when-idle: minutes", None),
                )
            }));
        };
        match lc.host.idle_s() {
            Some(s) if s >= min * 60 => {}
            got => {
                let why = match got {
                    Some(s) => format!("the Mac was used {} min ago (needs {min} min idle); not restarting Grok Bot now", s / 60),
                    None => "the Mac's idle time could not be read; not restarting Grok Bot".into(),
                };
                return Some(Box::new(move |out| {
                    let mut o = Outcome::fail(
                        ExitCode::StillWorking,
                        "launch",
                        why,
                        Some("try again later".into()),
                    );
                    o.warnings = warnings;
                    fail_line(out, o)
                }));
            }
        }
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
    None
}

/// P3: `routines`, `usage`, `files`.
fn cmd_p3(a: &Args, out: &Out, policy: &Policy) -> u8 {
    let cmd = a.pos[0].as_str();
    let pos = |i: usize| a.pos.get(i).map(String::as_str).unwrap_or("");
    let usage_err = |m: &str| {
        fail_line(
            out,
            Outcome::fail(ExitCode::Other, cmd, m.to_string(), None),
        )
    };
    if cmd == "routines" && pos(1) == "audit" {
        return cmd_routines_audit(a, out, policy);
    }
    if cmd != "usage" && pos(1).is_empty() {
        return usage_err(&format!("usage: botport grok {cmd} <bot> [action] ..."));
    }
    let sub = if pos(2).is_empty() { "list" } else { pos(2) };
    let ok_sub = match cmd {
        "routines" => ["list", "add", "enable", "disable", "remove", "run"].contains(&sub),
        "files" => ["list", "download", "fetch", "upload", "delete"].contains(&sub),
        _ => true,
    };
    if !ok_sub {
        return usage_err(&format!("unknown {cmd} action {sub:?}"));
    }
    let mut e = match build_engine(a, policy, caller()) {
        Ok(e) => e,
        Err(o) => return fail_line(out, o),
    };
    let wopts = WaitOpts {
        timeout_s: match a.num("timeout", 300.0) {
            Ok(v) => v,
            Err(m) => return usage_err(&m),
        },
        settle_s: match a.num("settle", 8.0) {
            Ok(v) => v,
            Err(m) => return usage_err(&m),
        },
    };
    let change = |o: &Outcome<RoutineChange>, e: &Engine| {
        let j = if o.value.as_ref().is_some_and(|v| !v.messages.is_empty()) {
            wrap(&e.policy, outcome_json(o, None))
        } else {
            outcome_json(o, None)
        };
        out.emit(o, j, |v| {
            let mut l: Vec<String> = v.messages.iter().map(msg_line).collect();
            if let Some(r) = &v.routine {
                l.push(routine_line(r));
            }
            l
        })
    };
    match (cmd, sub) {
        ("usage", _) => {
            let o = e.usage();
            let j = outcome_json(&o, None);
            out.emit(&o, j, |u| {
                vec![format!(
                    "{}% used  plan {}  resets {}  on-demand {} cents{}  available {}",
                    u.percent_used,
                    if u.plan.is_empty() { "-" } else { &u.plan },
                    u.resets_at
                        .as_deref()
                        .map(crate::util::local_from_rfc3339)
                        .unwrap_or_else(|| "-".into()),
                    u.on_demand_cents,
                    u.on_demand_limit_cents
                        .map(|c| format!(" of {c}"))
                        .unwrap_or_default(),
                    u.available
                )]
            })
        }
        ("routines", "list") => {
            let o = e.routines(pos(1));
            let j = outcome_json(&o, Some("routines"));
            out.emit(&o, j, |v| v.iter().map(routine_line).collect())
        }
        ("routines", "enable" | "disable") => {
            if pos(3).is_empty() {
                return usage_err(&format!(
                    "usage: botport grok routines <bot> {sub} <routine>"
                ));
            }
            let o = e.routine_toggle(pos(1), pos(3), sub == "enable");
            change(&o, &e)
        }
        ("routines", "add") => {
            let spec = crate::routines::RoutineSpec {
                name: a.get("name").unwrap_or(pos(3)).to_string(),
                schedule: a.get("schedule").unwrap_or("").to_string(),
                tz: a.get("tz").map(str::to_string),
                prompt: a.get("prompt").unwrap_or("").to_string(),
                disabled: a.has("disabled"),
            };
            let o = e.routine_add(pos(1), &spec, &wopts);
            change(&o, &e)
        }
        ("routines", "remove") => {
            if pos(3).is_empty() {
                return usage_err(
                    "usage: botport grok routines <bot> remove <exact routine name or id> [--yes]",
                );
            }
            let o = e.routine_remove(pos(1), pos(3), None, a.has("yes"), &wopts);
            change(&o, &e)
        }
        ("routines", "run") => {
            if pos(3).is_empty() {
                return usage_err("usage: botport grok routines <bot> run <routine>");
            }
            let o = e.routine_run(pos(1), pos(3), &wopts);
            change(&o, &e)
        }
        ("files", "list") => {
            let o = e.files(pos(1));
            let j = outcome_json(&o, Some("files"));
            out.emit(&o, j, |v| v.iter().map(file_line).collect())
        }
        ("files", "download") => {
            let names: Vec<String> = a.pos.get(3..).unwrap_or_default().to_vec();
            let dir = PathBuf::from(a.get("out").unwrap_or("."));
            let o = e.file_download(pos(1), &names, &dir);
            let j = outcome_json(&o, Some("saved"));
            out.emit(&o, j, |v| {
                v.iter()
                    .map(|s| {
                        format!(
                            "{} -> {}  {} bytes  sha256 {}",
                            s.name, s.saved_to, s.bytes, s.sha256
                        )
                    })
                    .collect()
            })
        }
        ("files", "fetch") => {
            let dir = PathBuf::from(a.get("out").unwrap_or("."));
            let which = Some(pos(3)).filter(|x| !x.is_empty());
            let o = e.file_fetch(pos(1), which, &dir);
            let j = outcome_json(&o, None);
            out.emit(&o, j, |s| {
                vec![format!(
                    "{} -> {}  {} bytes  sha256 {}",
                    s.name, s.saved_to, s.bytes, s.sha256
                )]
            })
        }
        ("files", "upload") => {
            let paths: Vec<PathBuf> = a
                .pos
                .get(3..)
                .unwrap_or_default()
                .iter()
                .map(PathBuf::from)
                .collect();
            let o = e.file_upload(pos(1), &paths);
            let j = outcome_json(&o, Some("files"));
            out.emit(&o, j, |v| v.iter().map(file_line).collect())
        }
        ("files", "delete") => {
            if pos(3).is_empty() {
                return usage_err("usage: botport grok files <bot> delete <exact name> [--yes]");
            }
            let o = e.file_delete(pos(1), pos(3), None, a.has("yes"));
            let j = outcome_json(&o, Some("file"));
            out.emit(&o, j, |f| vec![file_line(f)])
        }
        _ => usage_err("unknown action"),
    }
}

/// The mirror pair names configured on this Mac (`$XDG_CONFIG_HOME/botport/mirror/<pair>.toml`).
fn mirror_pair_names() -> Vec<String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("botport/mirror");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    names
}

/// `routines audit [--used-by NAME]... [--json]`: read-only, lists only.
fn cmd_routines_audit(a: &Args, out: &Out, policy: &Policy) -> u8 {
    let mut e = match build_engine(a, policy, caller()) {
        Ok(e) => e,
        Err(o) => return fail_line(out, o),
    };
    let mut used = mirror_pair_names();
    used.extend(a.all("used-by").into_iter().map(str::to_string));
    let o = e.routines_audit(&used);
    let j = outcome_json(&o, Some("routines"));
    let n = |v: Option<u64>| v.map_or("?".to_string(), |n| n.to_string());
    out.emit(&o, j, |v| {
        v.iter()
            .map(|r| {
                format!(
                    "{} / {}  {}  {}  {}/week  ~{} turns/week  last {}  runs {}{}",
                    r.bot,
                    r.name,
                    if r.enabled { "enabled" } else { "disabled" },
                    r.schedule,
                    n(r.runs_per_week),
                    n(r.est_turns_week),
                    r.last_run
                        .as_deref()
                        .map(crate::util::local_from_rfc3339)
                        .unwrap_or_else(|| "-".into()),
                    r.runs,
                    r.flag
                        .as_deref()
                        .map(|f| format!("  FLAG: {f}"))
                        .unwrap_or_default()
                )
            })
            .collect()
    })
}

/// `status` under the fake transport: the fixture's version, no port.
fn fake_status(e: &mut Engine, policy: &Policy) -> Outcome<crate::lifecycle::Status> {
    match e.capabilities() {
        Ok(c) => {
            let v = c.app_version.clone().unwrap_or_default();
            let st = crate::lifecycle::Status {
                installed: true,
                version: Some(v.clone()),
                tested: c.tested,
                running: true,
                pid: None,
                port_open: false,
                port_by_botport: false,
                port_opened_at: None,
                port_expires_at: None,
                preset: policy.preset.as_str().into(),
                local_tool_permission: e.local_tool_permission(),
            };
            Outcome::ok(
                st,
                Proof::pass(
                    format!(
                        "Grok Bot {v} (fake transport); debug port closed; preset {}",
                        policy.preset.as_str()
                    ),
                    "fixtures loaded; self-check",
                    "self-check",
                ),
            )
        }
        Err(err) => Outcome::fail(ExitCode::Unavailable, "status", err.to_string(), None),
    }
}

/// `botport grok canary`: see [`crate::canary`]. Connecting is the first check (the debug port must still open);
/// a failed connect is reported (and filed) as the port step failing.
fn cmd_canary(a: &Args, out: &Out, policy: &Policy, home: &std::path::Path, fake: bool) -> u8 {
    if a.has("write") {
        return fail_line(
            out,
            Outcome::fail(
                ExitCode::Other,
                "canary --write",
                "the write canary needs create/delete (P4); this build runs the read-only canary",
                Some("botport grok canary".into()),
            ),
        );
    }
    let dir = home.join("canary");
    let o = match build_engine(a, policy, caller()) {
        Ok(mut e) => crate::canary::run(&mut e, &dir),
        Err(f) => {
            let version = if fake {
                None
            } else {
                MacHost::default().installed_version()
            };
            crate::canary::unreachable(
                &dir,
                version.as_deref().unwrap_or("unknown"),
                if fake { "fake" } else { "debug-port" },
                crate::util::wall_ms(),
                f,
            )
        }
    };
    let j = outcome_json(&o, None);
    out.emit(&o, j, |r| {
        r.steps
            .iter()
            .map(|s| {
                format!(
                    "{:5} {:6} {:20} {}",
                    if !s.ran {
                        "skip"
                    } else if s.pass {
                        "ok"
                    } else {
                        "FAIL"
                    },
                    s.name,
                    s.covers,
                    s.detail
                )
            })
            .collect()
    })
}

fn cmd_policy(a: &Args, out: &Out, policy: &mut Policy, home: &std::path::Path) -> u8 {
    let sub = a.pos.get(1).map(String::as_str).unwrap_or("show");
    let r: Result<String, String> = match sub {
        "show" => Ok(format!("preset {}", policy.preset.as_str())),
        "preset" => match a.pos.get(2).and_then(|p| Preset::parse(p)) {
            Some(p) => {
                *policy = Policy::preset(p);
                policy.save(home).map(|_| format!("preset {}", p.as_str()))
            }
            None => Err("usage: botport grok policy preset careful|balanced|fast".into()),
        },
        "set" => match (a.pos.get(2), a.pos.get(3)) {
            (Some(k), Some(v)) => policy
                .set(k, v)
                .and_then(|_| policy.save(home))
                .map(|_| format!("{k} = {v}")),
            _ => Err("usage: botport grok policy set KEY VALUE".into()),
        },
        _ => Err(format!("unknown policy command {sub:?}")),
    };
    let o = match r {
        Ok(what) => {
            // independent check: read the saved file back
            let check = match sub {
                "show" => "self-check".to_string(),
                _ => match Policy::load(home) {
                    Ok(p) if p == *policy => "saved policy file agrees".into(),
                    _ => {
                        return fail_line(
                            out,
                            Outcome::fail(
                                ExitCode::Other,
                                "policy",
                                "the saved policy file does not match",
                                None,
                            ),
                        )
                    }
                },
            };
            Outcome::ok(policy.clone(), Proof::pass(what, check, "policy-file"))
        }
        Err(e) => Outcome::fail(ExitCode::Other, "policy", e, None),
    };
    let j = outcome_json(&o, Some("policy"));
    out.emit(&o, j, |p| vec![toml::to_string(p).unwrap_or_default()])
}

mod sigint {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    static HIT: AtomicBool = AtomicBool::new(false);

    #[cfg(unix)]
    extern "C" fn on_sigint(_: i32) {
        HIT.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    extern "C" {
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }

    /// Ctrl-C sets `flag` (watch then prints its summary) instead of killing the process.
    pub fn install(flag: Arc<AtomicBool>) {
        #[cfg(unix)]
        // SAFETY: installs an async-signal-safe handler that only stores to a static atomic.
        unsafe {
            signal(2, on_sigint);
        }
        std::thread::spawn(move || loop {
            if HIT.load(Ordering::SeqCst) {
                flag.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
    }
}

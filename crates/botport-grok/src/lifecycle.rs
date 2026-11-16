//! launch / restore / status, the port file (0600) and the TTL guard (floors 1 and 5).
//!
//! The port is opened only while needed: `launch` restarts the app with `--remote-debugging-address=127.0.0.1
//! --remote-debugging-port=0` (a random port), checks with `lsof` that the only listener is the Grok main process
//! on loopback, writes `{port, pid, ws_path, opened_at_ms, ttl_s}` to `~/.botport/grok-port.json` (0600) and arms a
//! detached guard that restores the app at TTL expiry. The port number and websocket path are never printed,
//! logged or put in a proof.
use crate::clock::SharedClock;
use crate::engine::{ExitCode, Outcome, Proof};
use crate::policy::{floors, Policy};
use crate::util::{hhmm_local, rfc3339};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortFile {
    pub port: u16,
    pub pid: u32,
    pub ws_path: String,
    pub opened_at_ms: u64,
    pub ttl_s: u64,
}

impl PortFile {
    pub fn path(home: &Path) -> PathBuf {
        home.join("grok-port.json")
    }
    pub fn load(home: &Path) -> Option<Self> {
        let s = std::fs::read_to_string(Self::path(home)).ok()?;
        serde_json::from_str(&s).ok()
    }
    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        crate::util::write_private(
            &Self::path(home),
            serde_json::to_string(self).unwrap_or_default().as_bytes(),
        )
    }
    pub fn remove(home: &Path) {
        let _ = std::fs::remove_file(Self::path(home));
    }
    pub fn expires_ms(&self) -> u64 {
        // review L1: a hand-edited or old port file cannot overflow
        self.opened_at_ms
            .saturating_add(self.ttl_s.saturating_mul(1000))
    }
}

/// What the lifecycle needs from the machine. `MacHost` is the real one; tests use a fake.
pub trait AppHost {
    /// The installed app version (Info.plist), None when not installed.
    fn installed_version(&self) -> Option<String>;
    /// The Grok Bot main process.
    fn main_pid(&self) -> Option<u32>;
    /// Quit the app cleanly (SIGTERM to the main process) and wait for it to exit.
    fn quit(&mut self, pid: u32) -> Result<(), String>;
    /// Start the app; with `debug`, with the loopback random-port flags.
    fn start(&mut self, debug: bool) -> Result<(), String>;
    /// `DevToolsActivePort`: (port, browser websocket path).
    fn devtools_active_port(&self) -> Option<(u16, String)>;
    fn remove_devtools_active_port(&mut self);
    /// TCP listeners of `pid`, as `addr:port` strings (lsof).
    fn tcp_listeners(&self, pid: u32) -> Vec<String>;
    /// Arm the TTL guard (a detached `botport grok _ttl-guard`).
    fn arm_guard(&mut self, opened_at_ms: u64, expires_ms: u64) -> Result<(), String>;
    /// Seconds since the last keyboard/mouse input on this Mac (`ioreg` HIDIdleTime); None if unknown.
    /// Additive (issue 02): `launch --when-idle` reads it.
    fn idle_s(&self) -> Option<u64> {
        None
    }
}

/// Review L7: the lock that serializes launch / restore / TTL enforcement across botport processes (and, through
/// `Engine::page_lock`, each transport operation on the page).
pub fn home_lock(home: &Path) -> PathBuf {
    home.join("grok.lock")
}

/// The marker a launch writes before starting the app with the port and removes once the port file is saved.
pub fn launching_marker(home: &Path) -> PathBuf {
    home.join("grok-launching")
}

/// What an expired or orphaned port window left behind, restored by [`Lifecycle::enforce_ttl_why`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leftover {
    /// The port file's TTL had passed (the job or its guard died).
    Expired,
    /// The app listens with the debug flags but there is no port file (a launch crashed before writing it).
    Orphan,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub installed: bool,
    pub version: Option<String>,
    pub tested: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub port_open: bool,
    pub port_by_botport: bool,
    pub port_opened_at: Option<String>,
    pub port_expires_at: Option<String>,
    pub preset: String,
    /// The app's "Execution on Local Computer" setting (`always` | `ask` | `never`), read through
    /// `getLocalToolPermission` when the debug port is open; left out of the JSON when it cannot be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_tool_permission: Option<String>,
}

pub struct Lifecycle<'a> {
    pub host: &'a mut dyn AppHost,
    pub home: PathBuf,
    pub clock: SharedClock,
}

const WAIT_PORT_MS: u64 = 20_000;

/// The longest port window botport opens (review L1): 24 h. Longer `port_ttl`/`--for` values are refused, and a
/// policy file holding one is clamped at launch.
pub const MAX_PORT_TTL_S: u64 = 24 * 3600;

impl Lifecycle<'_> {
    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Best effort: a home that cannot hold a lock file still works, unserialized.
    fn lock(&self) -> Option<std::fs::File> {
        crate::util::lock_file(&home_lock(&self.home)).ok()
    }

    pub fn status(&mut self, policy: &Policy) -> Outcome<Status> {
        let version = self.host.installed_version();
        let pid = self.host.main_pid();
        let pf = PortFile::load(&self.home);
        let dap = self.host.devtools_active_port();
        let listeners = pid.map(|p| self.host.tcp_listeners(p)).unwrap_or_default();
        let port_open = !listeners.is_empty();
        let st = Status {
            installed: version.is_some(),
            tested: version.as_deref().is_some_and(crate::version::is_tested),
            version: version.clone(),
            running: pid.is_some(),
            pid,
            port_open,
            port_by_botport: pf.as_ref().is_some_and(|f| Some(f.pid) == pid),
            port_opened_at: pf.as_ref().map(|f| rfc3339(f.opened_at_ms)),
            port_expires_at: pf.as_ref().map(|f| rfc3339(f.expires_ms())),
            preset: policy.preset.as_str().into(),
            local_tool_permission: None,
        };
        let Some(v) = version else {
            return Outcome::fail(
                ExitCode::Unavailable,
                "Grok Bot",
                "not installed",
                Some("install the Grok Bot desktop app".into()),
            );
        };
        let what = format!(
            "Grok Bot {v} {}; debug port {}; preset {}",
            if pid.is_some() {
                "running"
            } else {
                "not running"
            },
            match (&pf, port_open) {
                (Some(f), true) => format!("open until {}", hhmm_local(f.expires_ms())),
                (None, true) => "OPEN (not opened by botport)".into(),
                _ => "closed".into(),
            },
            policy.preset.as_str()
        );
        // independent check: the listener table (lsof) agrees with DevToolsActivePort
        let agrees = port_open == dap.is_some() || (!port_open && pid.is_none());
        let mut o = if agrees {
            Outcome::ok(
                st,
                Proof::pass(
                    what,
                    "process table + DevToolsActivePort agree",
                    "process-table",
                ),
            )
        } else {
            let mut o = Outcome::ok(
                st,
                Proof::fail(what, "DevToolsActivePort and the listener table disagree"),
            );
            o.exit = ExitCode::Other;
            o
        };
        if !crate::version::is_tested(&v) {
            o.warnings.push(format!(
                "Grok Bot {v} is not a tested version; writes are gated"
            ));
        }
        o
    }

    /// Restart the app with the port open; PASS checked with `lsof`.
    pub fn launch(&mut self, ttl_s: u64) -> Outcome<PortFile> {
        let _lock = self.lock();
        let what = "launch";
        let ttl_s = ttl_s.clamp(1, MAX_PORT_TTL_S);
        if self.host.installed_version().is_none() {
            return Outcome::fail(
                ExitCode::Unavailable,
                what,
                "Grok Bot is not installed",
                None,
            );
        }
        if let Some(pid) = self.host.main_pid() {
            if let Err(e) = self.host.quit(pid) {
                return Outcome::fail(
                    ExitCode::Unavailable,
                    what,
                    format!("could not quit Grok Bot: {e}"),
                    None,
                );
            }
        }
        self.host.remove_devtools_active_port();
        // crash marker: if this process dies before the port file is written, the next command restores
        let _ = crate::util::write_private(&launching_marker(&self.home), b"");
        if let Err(e) = self.host.start(true) {
            return Outcome::fail(
                ExitCode::Unavailable,
                what,
                format!("could not start Grok Bot: {e}"),
                None,
            );
        }
        let t0 = self.now();
        let (port, ws_path, pid) = loop {
            if let (Some((port, ws)), Some(pid)) =
                (self.host.devtools_active_port(), self.host.main_pid())
            {
                if !self.host.tcp_listeners(pid).is_empty() {
                    break (port, ws, pid);
                }
            }
            if self.now() - t0 > WAIT_PORT_MS {
                return Outcome::fail(
                    ExitCode::Unavailable,
                    what,
                    "the debug port did not open in 20 s",
                    Some("botport grok restore".into()),
                );
            }
            self.clock.sleep_ms(250);
        };
        let listeners = self.host.tcp_listeners(pid);
        // floor 1: every listener on loopback, and ours is among them
        if listeners.iter().any(|l| !floors::loopback_only(l))
            || !listeners.iter().any(|l| l.ends_with(&format!(":{port}")))
        {
            let _ = self.restore_unlocked();
            return Outcome::fail(
                ExitCode::Refused,
                what,
                "the app listened on a non-loopback address; restored (floor 1)",
                None,
            );
        }
        let pf = PortFile {
            port,
            pid,
            ws_path,
            opened_at_ms: self.now(),
            ttl_s,
        };
        if let Err(e) = pf.save(&self.home) {
            let _ = self.restore_unlocked();
            return Outcome::fail(
                ExitCode::Other,
                what,
                format!("could not write the port file: {e}; restored"),
                None,
            );
        }
        let _ = std::fs::remove_file(launching_marker(&self.home));
        let mut warnings = vec![];
        if let Err(e) = self.host.arm_guard(pf.opened_at_ms, pf.expires_ms()) {
            warnings.push(format!(
                "TTL guard not armed ({e}); every botport grok command still restores after expiry"
            ));
        }
        let p = Proof::pass(
            format!(
                "port open on 127.0.0.1 until {}",
                hhmm_local(pf.expires_ms())
            ),
            format!("listener is Grok Bot pid {pid}"),
            "lsof",
        );
        let mut o = Outcome::ok(pf, p);
        o.warnings = warnings;
        o
    }

    /// Restart the app with no flags and delete the port file; PASS checked with `lsof`.
    pub fn restore(&mut self) -> Outcome<()> {
        let _lock = self.lock();
        self.restore_unlocked()
    }

    fn restore_unlocked(&mut self) -> Outcome<()> {
        let what = "restore";
        if let Some(pid) = self.host.main_pid() {
            if let Err(e) = self.host.quit(pid) {
                return Outcome::fail(
                    ExitCode::Unavailable,
                    what,
                    format!("could not quit Grok Bot: {e}"),
                    None,
                );
            }
        }
        self.host.remove_devtools_active_port();
        PortFile::remove(&self.home);
        let _ = std::fs::remove_file(launching_marker(&self.home));
        if self.host.installed_version().is_some() {
            if let Err(e) = self.host.start(false) {
                return Outcome::fail(
                    ExitCode::Unavailable,
                    what,
                    format!("could not start Grok Bot: {e}"),
                    None,
                );
            }
        }
        let t0 = self.now();
        loop {
            let pid = self.host.main_pid();
            let listening = pid
                .map(|p| !self.host.tcp_listeners(p).is_empty())
                .unwrap_or(false);
            if !listening {
                return Outcome::ok(
                    (),
                    Proof::pass("port closed", "no Grok TCP listener", "lsof"),
                );
            }
            if self.now() - t0 > 10_000 {
                return Outcome::fail(
                    ExitCode::Other,
                    what,
                    "Grok Bot still has a TCP listener",
                    None,
                );
            }
            self.clock.sleep_ms(250);
        }
    }

    /// Floor 5 at every command: an expired port is restored before anything else runs.
    /// Returns true when it restored.
    pub fn enforce_ttl(&mut self) -> bool {
        self.enforce_ttl_why().is_some()
    }

    /// [`Self::enforce_ttl`], also restoring an orphaned port (the app has a `DevToolsActivePort` and a listener
    /// but botport has no port file: a launch that crashed half way). Says which one it restored.
    pub fn enforce_ttl_why(&mut self) -> Option<Leftover> {
        // review L7: a launch in another process holds the lock from `start` to `pf.save`, so its marker is
        // never mistaken for an orphan here
        let _lock = self.lock();
        match PortFile::load(&self.home) {
            Some(pf) if self.now() >= pf.expires_ms() => {
                let _ = self.restore_unlocked();
                Some(Leftover::Expired)
            }
            Some(_) => None,
            // only a port botport itself was opening (its launch marker is there); a port someone else opened
            // by hand is left alone
            None if launching_marker(&self.home).exists() => {
                let _ = self.restore_unlocked();
                let _ = std::fs::remove_file(launching_marker(&self.home));
                Some(Leftover::Orphan)
            }
            None => None,
        }
    }

    /// The `_ttl-guard` body: sleep until expiry, then restore if the same port file is still there.
    pub fn guard(&mut self, opened_at_ms: u64) -> bool {
        loop {
            let Some(pf) = PortFile::load(&self.home) else {
                return false;
            };
            if pf.opened_at_ms != opened_at_ms {
                return false; // a newer launch owns the port now
            }
            let now = self.now();
            if now >= pf.expires_ms() {
                return self.restore().exit == ExitCode::Done;
            }
            self.clock.sleep_ms((pf.expires_ms() - now).min(30_000));
        }
    }
}

// ---------------------------------------------------------------- the real host (macOS)

pub struct MacHost {
    pub app: PathBuf,
    pub support: PathBuf,
}

impl Default for MacHost {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        MacHost {
            app: PathBuf::from("/Applications/Grok Bot.app"),
            support: home.join("Library/Application Support/Grok Bot"),
        }
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let o = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

impl AppHost for MacHost {
    fn idle_s(&self) -> Option<u64> {
        let out = std::process::Command::new("/usr/sbin/ioreg")
            .args(["-c", "IOHIDSystem", "-d", "4"])
            .output()
            .ok()?;
        parse_hid_idle_s(&String::from_utf8_lossy(&out.stdout))
    }

    fn installed_version(&self) -> Option<String> {
        let plist = self.app.join("Contents/Info.plist");
        if !plist.exists() {
            return None;
        }
        let v = run(
            "/usr/bin/plutil",
            &[
                "-extract",
                "CFBundleShortVersionString",
                "raw",
                "-o",
                "-",
                &plist.to_string_lossy(),
            ],
        )
        .ok()?;
        Some(v.trim().to_string()).filter(|v| !v.is_empty())
    }

    fn main_pid(&self) -> Option<u32> {
        let exe = self.app.join("Contents/MacOS/");
        let out = run("/bin/ps", &["-axo", "pid=,comm="]).ok()?;
        out.lines().find_map(|l| {
            let l = l.trim_start();
            let (pid, comm) = l.split_once(' ')?;
            let comm = comm.trim();
            (comm.starts_with(&*exe.to_string_lossy()) && !comm.contains("Helper"))
                .then(|| pid.parse().ok())?
        })
    }

    fn quit(&mut self, pid: u32) -> Result<(), String> {
        run("/bin/kill", &["-TERM", &pid.to_string()])?;
        for _ in 0..80 {
            if self.main_pid() != Some(pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        Err("still running 20 s after SIGTERM".into())
    }

    fn start(&mut self, debug: bool) -> Result<(), String> {
        let app = self.app.to_string_lossy().into_owned();
        let mut args = vec!["-a", app.as_str()];
        if debug {
            args.extend([
                "--args",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
            ]);
        }
        let st = std::process::Command::new("/usr/bin/open")
            .args(&args)
            .status()
            .map_err(|e| e.to_string())?;
        st.success()
            .then_some(())
            .ok_or_else(|| format!("open exited {st}"))
    }

    fn devtools_active_port(&self) -> Option<(u16, String)> {
        let s = std::fs::read_to_string(self.support.join("DevToolsActivePort")).ok()?;
        let mut l = s.lines();
        let port = l.next()?.trim().parse().ok()?;
        Some((port, l.next().unwrap_or("").trim().to_string()))
    }

    fn remove_devtools_active_port(&mut self) {
        let _ = std::fs::remove_file(self.support.join("DevToolsActivePort"));
    }

    fn tcp_listeners(&self, pid: u32) -> Vec<String> {
        let out = run(
            "/usr/sbin/lsof",
            &[
                "-nP",
                "-a",
                "-p",
                &pid.to_string(),
                "-iTCP",
                "-sTCP:LISTEN",
                "-Fn",
            ],
        )
        .unwrap_or_default();
        out.lines()
            .filter_map(|l| l.strip_prefix('n'))
            .map(str::to_string)
            .collect()
    }

    fn arm_guard(&mut self, opened_at_ms: u64, _expires_ms: u64) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        detach(&mut std::process::Command::new(exe))
            .args([
                "grok",
                "_ttl-guard",
                "--opened-at",
                &opened_at_ms.to_string(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Make `cmd` start in its own session and process group (`setsid` before exec, review M1): Ctrl-C on the
/// launching terminal, SIGHUP when it closes, or a supervisor killing the caller's process group no longer reach
/// the TTL guard. Additive.
pub fn detach(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        extern "C" {
            fn setsid() -> i32;
        }
        // SAFETY: setsid is async-signal-safe and touches no memory; it runs in the forked child before exec.
        unsafe {
            cmd.pre_exec(|| {
                if setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd
}

/// `"HIDIdleTime" = 123456789` (nanoseconds) in `ioreg -c IOHIDSystem` output -> seconds.
pub fn parse_hid_idle_s(s: &str) -> Option<u64> {
    s.lines()
        .find(|l| l.contains("\"HIDIdleTime\""))
        .and_then(|l| l.rsplit('=').next())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|ns| ns / 1_000_000_000)
}

#[cfg(all(test, unix))]
mod detach_tests {
    extern "C" {
        fn getpgid(pid: i32) -> i32;
        fn getsid(pid: i32) -> i32;
    }

    // review M1: the guard is not in its parent's process group or session
    #[test]
    fn a_detached_child_has_its_own_group_and_session() {
        let mut c = std::process::Command::new("sleep");
        c.arg("5");
        let mut child = super::detach(&mut c).spawn().unwrap();
        let pid = child.id() as i32;
        // SAFETY: plain syscalls on pids we own.
        let (cg, cs, pg, ps) = unsafe { (getpgid(pid), getsid(pid), getpgid(0), getsid(0)) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(cg, pid, "the child leads its own process group");
        assert_ne!(cg, pg);
        assert_eq!(cs, pid, "and its own session");
        assert_ne!(cs, ps);
    }
}

#[cfg(test)]
mod idle_tests {
    #[test]
    fn hid_idle_parses() {
        let s = "    | |   \"HIDIdleTime\" = 754000000000\n";
        assert_eq!(super::parse_hid_idle_s(s), Some(754));
        assert_eq!(super::parse_hid_idle_s("nothing"), None);
    }
}

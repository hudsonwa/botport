//! `botport grok canary`: the P5 update check. Run after every app update (and before writing to a version that
//! is not in the tested table).
//!
//! 1. The transport's route still opens ([`crate::Transport::port_check`]: on the debug port, the vendor DevTools
//!    gate has not closed it). If not, nothing else runs.
//! 2. Read-only probes, one per command, against the live app (or the fixtures with `--transport fake`):
//!    status (version probe), bots (props + sidebar rows), state (ask/wait/watch settle polls), read, and the
//!    send/stop capabilities (present, not exercised: the write canary needs P4's create/delete).
//! 3. A local report, `<home>/canary/canary-<version>-<time>.json` (0600; counts only, no bot names, ids or text).
//!
//! A passing report unlocks writes on that version and transport under presets balanced and fast
//! ([`passed`], read by the engine's version gate). Careful stays blocked.
use crate::engine::{Engine, ExitCode, Outcome, Proof};
use crate::transport::{Cap, TransportError};
use crate::util::{rfc3339, write_private};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Step {
    /// `port`, `status`, `bots`, `state`, `read`, `send`, `stop`.
    pub name: String,
    /// The CLI command(s) this probe stands for.
    pub covers: String,
    pub pass: bool,
    /// False when policy kept the probe from running (it does not count either way).
    pub ran: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CanaryReport {
    pub version: String,
    /// The version is in the tested table ([`crate::version::TESTED`]).
    pub tested: bool,
    pub transport: String,
    /// RFC 3339 UTC.
    pub at: String,
    pub pass: bool,
    pub read_passed: usize,
    pub read_total: usize,
    pub steps: Vec<Step>,
    /// The report's file name under `<home>/canary/` (None if it could not be written).
    pub report: Option<String>,
}

fn step(name: &str, covers: &str, r: Result<String, String>) -> Step {
    let (pass, detail) = match r {
        Ok(d) => (true, d),
        Err(d) => (false, d),
    };
    Step {
        name: name.into(),
        covers: covers.into(),
        pass,
        ran: true,
        detail: crate::policy::floors::scrub(&detail),
    }
}

fn safe(v: &str) -> String {
    v.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Run the canary on `e` and write the report under `dir` (normally `~/.botport/canary`).
pub fn run(e: &mut Engine, dir: &Path) -> Outcome<CanaryReport> {
    let at_ms = e.clock.now_ms();
    let transport = e.transport_name().to_string();
    let mut steps = Vec::new();
    let mut unavailable = false;

    // 1. the route still opens
    let port = e.port_check();
    if let Err(TransportError::Unavailable(_) | TransportError::Unsupported(_)) = &port {
        unavailable = true;
    }
    steps.push(step(
        "port",
        "launch / connect",
        port.map_err(|err| err.to_string()),
    ));
    let caps = e.capabilities().ok();
    let version = caps
        .as_ref()
        .and_then(|c| c.app_version.clone())
        .unwrap_or_else(|| "unknown".into());
    let tested = crate::version::is_tested(&version);

    if steps[0].pass {
        // 2. read-only probes
        steps.push(step(
            "status",
            "status",
            match &caps {
                Some(c) => Ok(format!(
                    "Grok Bot {version}; {} in the tested-versions table; {} capabilities",
                    if c.tested { "listed" } else { "not listed" },
                    c.caps.len()
                )),
                None => Err("probe failed".into()),
            },
        ));
        let bots = e.bots();
        let list = bots.value.clone().unwrap_or_default();
        steps.push(step(
            "bots",
            "bots",
            if bots.exit == ExitCode::Done {
                Ok(format!(
                    "{} bots; sidebar rows agree ({})",
                    list.len(),
                    bots.proof.route
                ))
            } else {
                Err(bots.proof.line())
            },
        ));
        let mut ok = 0;
        let mut bad = None;
        for b in &list {
            match e.with_t(Cap::StateFlags, |t| t.bot_state(&b.id)) {
                Ok(_) => ok += 1,
                Err(err) => {
                    bad = Some(err.to_string());
                    break;
                }
            }
        }
        steps.push(step(
            "state",
            "ask / wait / watch",
            match bad {
                None if ok > 0 => Ok(format!("{ok} state snapshots parse")),
                None => Err("no bots to poll".into()),
                Some(m) => Err(m),
            },
        ));
        match list.iter().find(|b| b.main).or(list.first()) {
            Some(b) => {
                let r = e.read(&b.id, None, 5);
                let mut s = step(
                    "read",
                    "read",
                    if r.exit == ExitCode::Done {
                        Ok(format!(
                            "{} messages parse",
                            r.value.as_ref().map_or(0, Vec::len)
                        ))
                    } else {
                        Err(r.proof.line())
                    },
                );
                if r.exit == ExitCode::Refused {
                    s.ran = false;
                    s.detail = format!("skipped: {}", s.detail);
                }
                steps.push(s);
            }
            None => steps.push(step("read", "read", Err("no bot to read".into()))),
        }
        for (name, covers, cap) in [
            ("send", "send / ask", Cap::Send),
            ("stop", "stop", Cap::Stop),
        ] {
            steps.push(step(
                name,
                covers,
                match &caps {
                    Some(c) if c.caps.contains(&cap) => {
                        Ok("capability present (not exercised: read-only canary)".into())
                    }
                    _ => Err(format!("capability {cap:?} missing")),
                },
            ));
        }
    }

    let counted: Vec<&Step> = steps.iter().filter(|s| s.ran).collect();
    let read_total = counted.len();
    let read_passed = counted.iter().filter(|s| s.pass).count();
    let pass = read_total > 0 && read_passed == read_total;
    let mut rep = CanaryReport {
        version: version.clone(),
        tested,
        transport,
        at: rfc3339(at_ms),
        pass,
        read_passed,
        read_total,
        steps,
        report: None,
    };
    let stamp = rfc3339(at_ms).replace([':', '-'], "");
    let name = format!("canary-{}-{stamp}.json", safe(&version));
    let path: PathBuf = dir.join(&name);
    let written = crate::util::private_dir(dir).and_then(|_| {
        rep.report = Some(name.clone());
        let body = serde_json::to_vec_pretty(&rep).map_err(std::io::Error::other)?;
        write_private(&path, &body)
    });
    let mut warnings = vec![];
    if let Err(err) = written {
        rep.report = None;
        warnings.push(format!("canary report not written: {err}"));
    }
    if !tested && pass {
        warnings.push(format!(
            "Grok Bot {version} is not in the tested-versions table; this canary unlocks writes on it (balanced, fast)"
        ));
    }
    let what = format!("canary {version} read {read_passed}/{read_total}");
    let where_ = rep
        .report
        .as_ref()
        .map(|n| format!("report canary/{n}"))
        .unwrap_or_else(|| "report not written".into());
    let mut o = if pass {
        Outcome::ok(rep, Proof::pass(what, where_, "canary"))
    } else {
        let first = rep
            .steps
            .iter()
            .find(|s| s.ran && !s.pass)
            .map(|s| format!("{} failed: {}", s.name, s.detail))
            .unwrap_or_default();
        Outcome {
            value: Some(rep),
            exit: if unavailable {
                ExitCode::Unavailable
            } else {
                ExitCode::Other
            },
            proof: Proof::fail(what, format!("{first}; {where_}")),
            hint: Some(if unavailable {
                "the debug port did not open or answer: botport grok restore; botport grok launch; then rerun the canary".into()
            } else {
                "an app update changed a shape the parsers read; writes stay blocked on this version".into()
            }),
            warnings: vec![],
        }
    };
    o.warnings.extend(warnings);
    o
}

/// The connect itself failed (the port did not open, or the transport refused): file a failed report for it.
pub fn unreachable(
    dir: &Path,
    version: &str,
    transport: &str,
    at_ms: u64,
    f: Outcome<()>,
) -> Outcome<CanaryReport> {
    let mut rep = CanaryReport {
        version: version.into(),
        tested: crate::version::is_tested(version),
        transport: transport.into(),
        at: rfc3339(at_ms),
        pass: false,
        read_passed: 0,
        read_total: 1,
        steps: vec![step("port", "launch / connect", Err(f.proof.check.clone()))],
        report: None,
    };
    let name = format!(
        "canary-{}-{}.json",
        safe(version),
        rfc3339(at_ms).replace([':', '-'], "")
    );
    rep.report = Some(name.clone());
    let w = serde_json::to_vec_pretty(&rep)
        .map_err(std::io::Error::other)
        .and_then(|b| {
            crate::util::private_dir(dir)?;
            write_private(&dir.join(&name), &b)
        });
    if w.is_err() {
        rep.report = None;
    }
    let where_ = rep
        .report
        .as_ref()
        .map(|n| format!("report canary/{n}"))
        .unwrap_or_else(|| "report not written".into());
    Outcome {
        value: Some(rep),
        exit: f.exit,
        proof: Proof::fail(
            format!("canary {version} read 0/1"),
            format!("port failed: {}; {where_}", f.proof.check),
        ),
        hint: f.hint,
        warnings: f.warnings,
    }
}

/// The time (RFC 3339) of a passing canary on `version` over `transport` in `dir`, if any. A fake-transport
/// canary never unlocks the debug port, and the other way round.
pub fn passed(dir: &Path, version: &str, transport: &str) -> Option<String> {
    let prefix = format!("canary-{}-", safe(version));
    let mut best: Option<String> = None;
    for ent in std::fs::read_dir(dir).ok()?.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(ent.path()) else {
            continue;
        };
        let Ok(r) = serde_json::from_slice::<CanaryReport>(&bytes) else {
            continue;
        };
        if r.pass && r.version == version && r.transport == transport && best.as_ref() < Some(&r.at)
        {
            best = Some(r.at);
        }
    }
    best
}

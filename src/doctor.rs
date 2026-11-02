//! `botport doctor`: can this machine run botport against Hermes? Each check prints `[ok]` or `[FAIL] … — fix: …`;
//! exit 0 when every check passes, 1 when any fails (2 stays the usage-error code).
use crate::adapters::hermes::spec;
use crate::core::util::Args;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Oldest Hermes Agent release botport is tested against (docs/INSTALL.md states the same).
pub const MIN_HERMES: (u32, u32, u32) = (0, 22, 0);

pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn check(name: &'static str, r: Result<String, String>) -> Check {
    match r {
        Ok(detail) => Check {
            name,
            ok: true,
            detail,
        },
        Err(detail) => Check {
            name,
            ok: false,
            detail,
        },
    }
}

/// First `X.Y.Z` in a `hermes --version` line.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let re = regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").ok()?;
    let c = re.captures(s)?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?))
}

/// Can `bin` be started at all? The same message as doctor's `hermes` check when it cannot.
pub fn runnable(bin: &str) -> Result<std::process::Output, String> {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!("`{bin}` not runnable ({e}) — fix: install Hermes Agent and put `hermes` on PATH, or pass --hermes-bin PATH")
        })
}

fn hermes(bin: &str) -> Result<String, String> {
    let o = runnable(bin)?;
    let line = String::from_utf8_lossy(&o.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !o.status.success() {
        return Err(format!(
            "`{bin} --version` exited {} — fix: repair the Hermes install",
            o.status
        ));
    }
    let (a, b, c) = MIN_HERMES;
    match parse_version(&line) {
        Some(v) if v >= MIN_HERMES => Ok(line),
        Some(_) => Err(format!(
            "{line}: older than {a}.{b}.{c} — fix: `hermes update`"
        )),
        None => Err(format!(
            "cannot read a version from {line:?} — fix: check `{bin} --version`"
        )),
    }
}

fn writable(dir: &Path) -> Result<String, String> {
    let probe = dir.join(format!(".botport-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .and_then(|_| std::fs::remove_file(&probe))
        .map(|_| format!("{} is writable", dir.display()))
        .map_err(|e| {
            format!(
                "cannot write in {} ({e}) — fix: check its owner and permissions",
                dir.display()
            )
        })
}

pub fn run(a: &Args, hermes_home: &Path) -> Vec<Check> {
    let bin = a.get("hermes-bin").unwrap_or("hermes");
    let mut out = vec![check("hermes", hermes(bin))];
    let dir: PathBuf = match (a.path("profile-dir"), a.get("profile")) {
        (Some(d), _) => d,
        (None, Some("default")) | (None, None) => hermes_home.to_path_buf(),
        (None, Some(p)) => hermes_home.join("profiles").join(p),
    };
    let what = if a.path("profile-dir").is_some() || a.get("profile").is_some() {
        "profile dir"
    } else {
        "hermes home"
    };
    let exists = dir.is_dir();
    out.push(check(
        if what == "profile dir" { "profile dir" } else { "hermes home" },
        if exists {
            Ok(dir.display().to_string())
        } else if what == "profile dir" && a.path("spec").is_some() {
            Ok(format!("{} missing (apply will create it)", dir.display()))
        } else if what == "profile dir" {
            Err(format!("{} missing — fix: `hermes profile create NAME`, or pass --hermes-home/--profile-dir", dir.display()))
        } else {
            Err(format!("{} missing — fix: run `hermes` once, or set HERMES_HOME / --hermes-home", dir.display()))
        },
    ));
    if exists {
        out.push(check("write", writable(&dir)));
    }
    if let Some(p) = a.path("spec") {
        out.push(check(
            "spec",
            spec::Spec::load(&p, hermes_home)
                .map(|_| format!("{} parses", p.display()))
                .map_err(|e| format!("{e} — fix: see docs/USER-GUIDE.md for the spec format")),
        ));
    }
    out
}

pub fn lines_json(cs: &[Check]) -> (Vec<String>, Value) {
    let lines = cs
        .iter()
        .map(|c| {
            format!(
                "[{}] {}: {}",
                if c.ok { "ok" } else { "FAIL" },
                c.name,
                c.detail
            )
        })
        .collect();
    let checks: Vec<Value> = cs
        .iter()
        .map(|c| json!({"check": c.name, "ok": c.ok, "detail": c.detail}))
        .collect();
    (
        lines,
        json!({"command": "doctor", "ok": cs.iter().all(|c| c.ok), "checks": checks}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(
            parse_version("Hermes Agent v0.22.5 (2026.9.24) · stable"),
            Some((0, 22, 5))
        );
        assert_eq!(parse_version("hermes dev"), None);
        assert!(parse_version("v0.21.9").unwrap() < MIN_HERMES);
    }
}

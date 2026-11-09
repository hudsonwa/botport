//! The CI entry points exist and run the same checks; docs keep the isolation line safe.
use std::fs;
use std::path::Path;

const CHECKS: [&str; 3] = [
    "cargo test --workspace",
    "cargo clippy --workspace --all-targets -- -D warnings",
    "cargo fmt --all --check",
];

fn read(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn ci_script_runs_every_check() {
    let s = read("scripts/ci.sh");
    assert!(s.starts_with("#!"), "ci.sh needs a shebang");
    assert!(s.contains("set -euo pipefail"));
    for c in CHECKS {
        assert!(s.contains(c), "ci.sh misses `{c}`");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/ci.sh");
        assert!(
            fs::metadata(p).unwrap().permissions().mode() & 0o111 != 0,
            "ci.sh not executable"
        );
    }
}

#[test]
fn workflows_call_the_ci_script() {
    for wf in [".forgejo/workflows/ci.yml", ".github/workflows/ci.yml"] {
        let s = read(wf);
        assert!(
            s.contains("scripts/ci.sh"),
            "{wf} does not run scripts/ci.sh"
        );
        assert!(s.contains("pull_request"), "{wf} does not run on PRs");
    }
}

fn md_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() {
            if name != "target" && name != ".git" {
                md_files(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

/// `export HOME=… G2H_HOME="$HOME/…"` expands `$HOME` before HOME changes, so G2H_HOME
/// would point at the real home. Docs must set them in two separate exports.
#[test]
fn docs_never_export_home_and_g2h_home_together() {
    let mut files = Vec::new();
    md_files(Path::new(env!("CARGO_MANIFEST_DIR")), &mut files);
    assert!(!files.is_empty());
    for f in files {
        let s = fs::read_to_string(&f).unwrap();
        for (i, line) in s.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("export ") && t.contains("HOME=") && t.contains("G2H_HOME=") {
                let rest = t.replace("G2H_HOME=", "");
                assert!(
                    !rest.contains(" HOME="),
                    "{}:{}: HOME and G2H_HOME in one export",
                    f.display(),
                    i + 1
                );
            }
        }
    }
}

#[test]
fn github_actions_are_pinned_by_commit_sha() {
    for wf in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        for line in read(wf).lines().filter(|l| l.contains("uses:")) {
            let r = line.split("uses:").nth(1).unwrap().trim();
            if r.starts_with("./") {
                continue; // a workflow in this repo, pinned by the commit itself
            }
            let sha = r
                .split('@')
                .nth(1)
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("");
            assert!(
                sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
                "{wf}: not pinned by SHA: {line}"
            );
        }
    }
}

/// install.sh against invented archives in a temp folder (HOME and the bin folder inside it): a plain archive
/// installs; one holding a link or a member outside botport-<target>/ is refused before anything is unpacked.
#[cfg(unix)]
#[test]
fn install_sh_refuses_links_and_unexpected_members() {
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    use std::process::Command;
    let t = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", a) => format!("{a}-apple-darwin"),
        ("linux", a) => format!("{a}-unknown-linux-musl"),
        _ => return,
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let root = std::env::temp_dir().join(format!("botport-install-{}-{nanos}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let run = |case: &str| -> (bool, String, bool) {
        let w = root.join(case);
        let stage = w.join(format!("src/botport-{t}"));
        fs::create_dir_all(stage.join("LICENSES")).unwrap();
        fs::create_dir_all(w.join("srv")).unwrap();
        for b in ["botport", "grok2hermes", "g2h", "g2hd"] {
            fs::write(stage.join(b), "#!/bin/sh\necho demo\n").unwrap();
            fs::set_permissions(stage.join(b), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(stage.join("LICENSES/demo.MIT"), "demo\n").unwrap();
        let mut members = vec![format!("botport-{t}")];
        match case {
            "link" => symlink("/etc/hosts", stage.join("hosts")).unwrap(),
            "other" => {
                fs::write(w.join("src/other.txt"), "demo\n").unwrap();
                members.push("other.txt".into());
            }
            _ => {}
        }
        let archive = format!("botport-{t}.tar.gz");
        assert!(Command::new("tar")
            .arg("-czf")
            .arg(w.join("srv").join(&archive))
            .args(&members)
            .current_dir(w.join("src"))
            .status()
            .unwrap()
            .success());
        let sum = Command::new("sh")
            .arg("-c")
            .arg("shasum -a 256 \"$1\" 2>/dev/null || sha256sum \"$1\"")
            .arg("sh")
            .arg(&archive)
            .current_dir(w.join("srv"))
            .output()
            .unwrap();
        fs::write(w.join("srv").join(format!("{archive}.sha256")), &sum.stdout).unwrap();
        let o = Command::new("sh")
            .arg(&script)
            .env("HOME", w.join("home"))
            .env("BOTPORT_BIN_DIR", w.join("home/bin"))
            .env(
                "BOTPORT_BASE_URL",
                format!("file://{}", w.join("srv").display()),
            )
            .output()
            .unwrap();
        let installed = w.join("home/bin/botport").exists();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            installed,
        )
    };
    let (ok, e, installed) = run("plain");
    assert!(ok && installed, "{e}");
    let (ok, e, installed) = run("link");
    assert!(!ok && !installed && e.contains("holds a link"), "{e}");
    let (ok, e, installed) = run("other");
    assert!(
        !ok && !installed && e.contains("unexpected member: other.txt"),
        "{e}"
    );
    fs::remove_dir_all(&root).ok();
}

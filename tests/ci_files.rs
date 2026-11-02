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

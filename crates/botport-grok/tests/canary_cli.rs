//! `botport grok canary --transport fake`: fully offline through the CLI entry point (own process: sets
//! BOTPORT_HOME).
mod common;
use common::*;

#[test]
fn cli_canary_runs_offline_on_the_fixtures_and_files_its_report() {
    let s = Scratch::new("canary-cli");
    std::env::set_var("BOTPORT_HOME", &s.0);
    let fx = fx();
    let args = |extra: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = ["canary", "--transport", "fake", "--fixtures"]
            .iter()
            .map(|x| x.to_string())
            .collect();
        v.push(fx.to_string_lossy().into_owned());
        v.extend(extra.iter().map(|x| x.to_string()));
        v
    };
    assert_eq!(botport_grok::cli::main(&args(&["--json", "--no-log"])), 0);
    let dir = s.0.join("canary");
    let n = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(n, 1);
    // --write is not built yet (P4): exit 1, nothing filed
    assert_eq!(botport_grok::cli::main(&args(&["--write"])), 1);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    // human output works too (second report; names carry a seconds stamp, so allow a same-second overwrite)
    assert_eq!(botport_grok::cli::main(&args(&["--no-log"])), 0);
    assert!(std::fs::read_dir(&dir).unwrap().count() >= 1);
}

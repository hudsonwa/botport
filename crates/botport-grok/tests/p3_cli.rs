//! `botport grok routines|usage|files --transport fake` through the CLI entry point (own process: BOTPORT_HOME).
mod common;
use common::*;

#[test]
fn cli_p3_commands_run_offline() {
    let s = Scratch::new("p3-cli");
    std::env::set_var("BOTPORT_HOME", s.0.join("home"));
    let fx = fx().to_string_lossy().into_owned();
    let run = |a: &[&str]| -> u8 {
        let mut v: Vec<String> = a.iter().map(|x| x.to_string()).collect();
        v.extend(["--transport", "fake", "--fixtures", &fx, "--no-log"].map(String::from));
        botport_grok::cli::main(&v)
    };
    let bot2 = "00000000-0000-4000-8000-000000000002";
    let out = s.0.join("dl").to_string_lossy().into_owned();
    assert_eq!(run(&["usage", "--json"]), 0);
    assert_eq!(run(&["routines", common::MAIN]), 0);
    assert_eq!(
        run(&[
            "routines",
            common::MAIN,
            "disable",
            "weekly-cost-log",
            "--json"
        ]),
        0
    );
    assert_eq!(run(&["routines", common::MAIN, "explode"]), 1);
    assert_eq!(run(&["files", bot2, "--json"]), 0);
    assert_eq!(
        run(&[
            "files",
            bot2,
            "download",
            "export.json",
            "--out",
            &out,
            "--json"
        ]),
        0
    );
    assert_eq!(
        run(&["files", bot2, "download", "export.json", "--out", &out]),
        0
    );
    assert!(s.0.join("dl/export.json").exists() && s.0.join("dl/export (1).json").exists());
    assert_eq!(run(&["files", bot2, "delete", "export"]), 4);
    assert_eq!(run(&["files"]), 1);
}

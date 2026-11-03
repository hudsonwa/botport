//! End-to-end tests of `botport mirror ...` on invented fixtures.
mod common;
use common::{s, w, Tmp};
use std::process::Command;

fn bin(state: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_botport"));
    c.env("XDG_STATE_HOME", state);
    c
}

#[test]
fn stats_summarises_the_metrics_file() {
    let t = Tmp::new("mirror-stats");
    let f = t.0.join("m.csv");
    let mut csv = String::from(botport::core::metrics::HEADER);
    csv.push('\n');
    for (i, ms) in [120, 80, 300].iter().enumerate() {
        csv.push_str(&format!(
            "2026-01-03T05:0{i}:00Z,demo-bot,mirror-sync,drive,drive,30,1,0,0,100,10,{ms},40,{},0,{}\n",
            ms - 40,
            20 + i
        ));
    }
    csv.push_str("not,a,row\n");
    w(&f, &csv);
    let o = bin(&t.0)
        .args(["mirror", "stats", "--metrics", s(&f), "--json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let g = &v["groups"][0];
    assert_eq!(
        (
            g["runs"].as_u64(),
            g["p50_ms"].as_u64(),
            g["p95_ms"].as_u64()
        ),
        (Some(3), Some(120), Some(300))
    );
    assert_eq!(g["local_tokens"], 0);
    // an old window leaves nothing
    let o = bin(&t.0)
        .args(["mirror", "stats", "--metrics", s(&f), "--since", "1m"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("no mirror runs"));
    let o = bin(&t.0)
        .args(["mirror", "stats", "--since", "7w"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    let o = bin(&t.0).args(["mirror", "nope"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
}

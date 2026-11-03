//! `botport mirror ...`: mirror mode's commands (docs/design/MIRROR.md section 4.8), as library functions the CLI
//! (src/main.rs) calls. Each returns an [`Out`]: an exit code, at most a few summary lines and a JSON report.
use crate::core::metrics;
use crate::core::util::{err, now_secs, Args, Res};
use serde_json::{json, Value};

/// What a mirror command reports.
pub struct Out {
    /// Exit code.
    pub code: u8,
    /// Summary lines for stdout.
    pub lines: Vec<String>,
    /// The JSON report.
    pub json: Value,
}

/// Run `botport mirror <sub>`.
pub fn run(a: &Args) -> Res<Out> {
    match a.pos.get(1).map(String::as_str) {
        Some("stats") => stats(a),
        Some(s) => err(format!("unknown mirror command {s:?} (try --help)")),
        None => err("mirror needs a command (try --help)"),
    }
}

/// `mirror stats [--since 7d] [--metrics FILE]`: count and p50/p95 time and payload per command and transport.
fn stats(a: &Args) -> Res<Out> {
    let file = a.path("metrics").unwrap_or_else(metrics::default_path);
    let since = match a.get("since") {
        Some(s) => Some(now_secs().saturating_sub(metrics::parse_since(s)?)),
        None => None,
    };
    let rows = metrics::read(&file)?;
    let groups = metrics::stats(&rows, since);
    let mut lines: Vec<String> = metrics::render(&groups).lines().map(String::from).collect();
    if groups.is_empty() {
        lines = vec![format!("no mirror runs recorded in {}", file.display())];
    }
    let json = json!({
        "file": file.display().to_string(),
        "groups": groups.iter().map(|g| json!({
            "cmd": g.cmd, "transport": g.transport, "runs": g.runs, "p50_ms": g.p50_ms, "p95_ms": g.p95_ms,
            "p50_payload_tokens_est": g.p50_tokens, "p95_payload_tokens_est": g.p95_tokens,
            "local_tokens": g.local_tokens,
        })).collect::<Vec<_>>(),
    });
    Ok(Out {
        code: 0,
        lines,
        json,
    })
}

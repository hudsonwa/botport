//! Time and payload figures for mirror runs: one CSV row per run, appended to a file that never leaves this machine.
//! Spec: docs/design/MIRROR.md section 8.
//!
//! A row holds counts, bytes, milliseconds and names (the pair, the command, the transport and the path it took):
//! never item content or bot text. `local_tokens` is the model tokens botport itself spent on the local side, always
//! 0 (it makes no model calls); `remote_payload_tokens_est` estimates what the other side's routine read and wrote
//! for botport: bytes divided by 3, rounded up (an over-estimate for English text).
//!
//! ```
//! use botport::core::metrics::{payload_tokens, Row};
//!
//! assert_eq!(payload_tokens(10), 4);
//! let r = Row { cmd: "mirror-sync".into(), wall_ms: 12, ..Row::default() };
//! assert_eq!(Row::parse(&r.line()).unwrap(), r);
//! ```
#![warn(missing_docs)]

use super::util::{err, parse_utc_iso, Res};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The CSV header (column order is part of the format).
pub const HEADER: &str = "ts,pair,cmd,transport,path,items,changed,conflicts,in_flight,read_bytes,written_bytes,wall_ms,net_ms,local_ms,local_tokens,remote_payload_tokens_est";

/// One run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    /// UTC time, `YYYY-MM-DDTHH:MM:SSZ`.
    pub ts: String,
    /// The pair (local profile name).
    pub pair: String,
    /// The command (`mirror-sync`, `digest-write`, ...).
    pub cmd: String,
    /// Transport id.
    pub transport: String,
    /// The path the run took (e.g. `local+drive`).
    pub path: String,
    /// Items planned.
    pub items: u64,
    /// Items written or sent.
    pub changed: u64,
    /// Open conflicts after the run.
    pub conflicts: u64,
    /// Items in flight after the run.
    pub in_flight: u64,
    /// Bytes read.
    pub read_bytes: u64,
    /// Bytes written.
    pub written_bytes: u64,
    /// Wall time, milliseconds.
    pub wall_ms: u64,
    /// Time waiting on the network, milliseconds.
    pub net_ms: u64,
    /// Time spent on this machine (`wall_ms - net_ms`): what the sub-second target is about.
    pub local_ms: u64,
    /// Model tokens botport spent on the local side (always 0).
    pub local_tokens: u64,
    /// Estimated payload tokens on the remote side; empty when it does not apply.
    pub remote_payload_tokens_est: Option<u64>,
}

/// Payload tokens for `bytes`: bytes / 3, rounded up.
pub fn payload_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(3)
}

/// A field safe to put in the CSV unquoted: commas, quotes and line breaks become `_`.
fn field(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if matches!(c, ',' | '"' | '\n' | '\r' | '\t') {
                '_'
            } else {
                c
            }
        })
        .collect();
    // a spreadsheet would read a cell starting with one of these as a formula
    if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s
    }
}

impl Row {
    /// The CSV line (no newline).
    pub fn line(&self) -> String {
        let n = [
            self.items,
            self.changed,
            self.conflicts,
            self.in_flight,
            self.read_bytes,
            self.written_bytes,
            self.wall_ms,
            self.net_ms,
            self.local_ms,
            self.local_tokens,
        ]
        .map(|x| x.to_string())
        .join(",");
        format!(
            "{},{},{},{},{},{n},{}",
            field(&self.ts),
            field(&self.pair),
            field(&self.cmd),
            field(&self.transport),
            field(&self.path),
            self.remote_payload_tokens_est
                .map(|x| x.to_string())
                .unwrap_or_default()
        )
    }

    /// Parse a CSV line written by [`Row::line`].
    pub fn parse(l: &str) -> Res<Row> {
        let f: Vec<&str> = l.split(',').collect();
        if f.len() != 16 {
            return err("metrics line has the wrong number of fields");
        }
        let n = |i: usize| {
            f[i].parse::<u64>()
                .map_err(|_| format!("metrics field {i} is not a number"))
        };
        Ok(Row {
            ts: f[0].into(),
            pair: f[1].into(),
            cmd: f[2].into(),
            transport: f[3].into(),
            path: f[4].into(),
            items: n(5)?,
            changed: n(6)?,
            conflicts: n(7)?,
            in_flight: n(8)?,
            read_bytes: n(9)?,
            written_bytes: n(10)?,
            wall_ms: n(11)?,
            net_ms: n(12)?,
            local_ms: n(13)?,
            local_tokens: n(14)?,
            remote_payload_tokens_est: if f[15].is_empty() { None } else { Some(n(15)?) },
        })
    }
}

/// The default metrics file: `$XDG_STATE_HOME/botport/metrics.csv`, else `~/.local/state/botport/metrics.csv`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| super::util::home().join(".local/state"));
    base.join("botport/metrics.csv")
}

/// Append one row (the header first when the file is new). File 0600, its directory 0700.
pub fn append(p: &Path, r: &Row) -> Res<()> {
    let dir = p.parent().ok_or("metrics file has no directory")?;
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
        .map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let before = fs::symlink_metadata(p).ok();
    if before.as_ref().is_some_and(|m| !m.is_file()) {
        return err(format!("{} is not a regular file", p.display()));
    }
    let new = before.is_none();
    let mut o = fs::OpenOptions::new();
    // a new file is created exclusively (never through a link someone placed meanwhile); an existing one must still be
    // the regular file seen a moment ago once it is open
    if new {
        o.append(true).create_new(true);
    } else {
        o.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o
        .open(p)
        .map_err(|e| format!("open {}: {e}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = f
            .metadata()
            .map_err(|e| format!("stat {}: {e}", p.display()))?;
        if let Some(b) = &before {
            if (b.dev(), b.ino()) != (m.dev(), m.ino()) {
                return err(format!("{} changed while it was opened", p.display()));
            }
        }
        // the owner's metrics are private, whatever mode an older file had
        if m.permissions().mode() & 0o077 != 0 {
            f.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod {}: {e}", p.display()))?;
        }
    }
    let mut s = String::new();
    if new {
        s.push_str(HEADER);
        s.push('\n');
    }
    s.push_str(&r.line());
    s.push('\n');
    f.write_all(s.as_bytes())
        .map_err(|e| format!("write {}: {e}", p.display()))
}

/// Every row in the file (malformed lines and the header skipped).
pub fn read(p: &Path) -> Res<Vec<Row>> {
    let t = match fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return err(format!("read {}: {e}", p.display())),
    };
    Ok(t.lines().filter_map(|l| Row::parse(l).ok()).collect())
}

/// Figures for one (command, transport) group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Command.
    pub cmd: String,
    /// Transport.
    pub transport: String,
    /// Runs.
    pub runs: usize,
    /// Median wall time (ms).
    pub p50_ms: u64,
    /// 95th percentile wall time (ms).
    pub p95_ms: u64,
    /// Median time spent on this machine (ms).
    pub p50_local_ms: u64,
    /// 95th percentile time spent on this machine (ms).
    pub p95_local_ms: u64,
    /// Median payload estimate, when any run had one.
    pub p50_tokens: Option<u64>,
    /// 95th percentile payload estimate.
    pub p95_tokens: Option<u64>,
    /// Local model tokens, summed (0 expected).
    pub local_tokens: u64,
}

/// The nearest-rank percentile of sorted values.
pub fn percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len() as u64).div_ceil(100).max(1) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Group rows at or after `since` (unix seconds; rows with an unreadable time are left out when `since` is set).
pub fn stats(rows: &[Row], since: Option<u64>) -> Vec<Group> {
    let mut g: BTreeMap<(String, String), Vec<&Row>> = BTreeMap::new();
    for r in rows {
        if let Some(s) = since {
            if parse_utc_iso(&r.ts).is_none_or(|t| t < s) {
                continue;
            }
        }
        g.entry((r.cmd.clone(), r.transport.clone()))
            .or_default()
            .push(r);
    }
    g.into_iter()
        .map(|((cmd, transport), rs)| {
            let mut ms: Vec<u64> = rs.iter().map(|r| r.wall_ms).collect();
            ms.sort_unstable();
            let mut lm: Vec<u64> = rs.iter().map(|r| r.local_ms).collect();
            lm.sort_unstable();
            let mut tk: Vec<u64> = rs
                .iter()
                .filter_map(|r| r.remote_payload_tokens_est)
                .collect();
            tk.sort_unstable();
            Group {
                cmd,
                transport,
                runs: rs.len(),
                p50_ms: percentile(&ms, 50),
                p95_ms: percentile(&ms, 95),
                p50_local_ms: percentile(&lm, 50),
                p95_local_ms: percentile(&lm, 95),
                p50_tokens: (!tk.is_empty()).then(|| percentile(&tk, 50)),
                p95_tokens: (!tk.is_empty()).then(|| percentile(&tk, 95)),
                local_tokens: rs.iter().map(|r| r.local_tokens).sum(),
            }
        })
        .collect()
}

/// `7d`, `12h`, `30m` -> seconds.
pub fn parse_since(s: &str) -> Res<u64> {
    let bad = || format!("invalid duration {s:?} (e.g. 7d, 12h, 30m)");
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: u64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let mult = match unit {
        'd' => 86400,
        'h' => 3600,
        'm' => 60,
        _ => return Err(bad()),
    };
    n.checked_mul(mult).ok_or_else(bad)
}

/// The `mirror stats` table.
pub fn render(groups: &[Group]) -> String {
    let mut out = String::from(
        "command          transport  runs  p50 ms  p95 ms  local p50/p95  payload p50/p95  local tokens\n",
    );
    for g in groups {
        let tk = match (g.p50_tokens, g.p95_tokens) {
            (Some(a), Some(b)) => format!("{a}/{b}"),
            _ => "-".into(),
        };
        out.push_str(&format!(
            "{:<16} {:<10} {:>4} {:>7} {:>7}  {:>13}  {:>15}  {:>12}\n",
            g.cmd,
            g.transport,
            g.runs,
            g.p50_ms,
            g.p95_ms,
            format!("{}/{}", g.p50_local_ms, g.p95_local_ms),
            tk,
            g.local_tokens
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn cells_never_read_as_formulas() {
        assert_eq!(super::field("=cmd"), "'=cmd");
        assert_eq!(super::field("@x,y"), "'@x_y");
        assert_eq!(super::field("demo-bot"), "demo-bot");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_metrics_file_is_made_private_and_a_link_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = crate::core::util::test_dir("metrics-mode");
        let f = d.join("metrics.csv");
        std::fs::write(&f, format!("{HEADER}\n")).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        append(&f, &row("2000-01-01T00:00:00Z", "mirror-sync", 1, None)).unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = d.join("linked.csv");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(append(&link, &row("2000-01-01T00:00:00Z", "mirror-sync", 1, None)).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("botport-metrics-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn row(ts: &str, cmd: &str, ms: u64, tok: Option<u64>) -> Row {
        Row {
            ts: ts.into(),
            pair: "demo-bot".into(),
            cmd: cmd.into(),
            transport: "drive".into(),
            path: "drive".into(),
            wall_ms: ms,
            remote_payload_tokens_est: tok,
            ..Row::default()
        }
    }

    #[test]
    fn lines_round_trip_and_fields_are_made_safe() {
        let r = row("2000-01-03T05:00:00Z", "mirror-sync", 212, Some(17));
        assert_eq!(Row::parse(&r.line()).unwrap(), r);
        let mut odd = r.clone();
        odd.pair = "demo,bot\n\"x\"".into();
        let l = odd.line();
        assert_eq!(l.split(',').count(), 16);
        assert!(!l.contains('\n'));
        assert_eq!(Row::parse(&l).unwrap().pair, "demo_bot__x_");
        assert!(Row::parse("1,2,3").is_err());
    }

    #[test]
    fn append_writes_the_header_once_and_is_private() {
        let d = tmp("append");
        let p = d.join("botport/metrics.csv");
        append(&p, &row("2000-01-03T05:00:00Z", "mirror-sync", 10, None)).unwrap();
        append(&p, &row("2000-01-03T05:01:00Z", "mirror-sync", 20, Some(5))).unwrap();
        let t = fs::read_to_string(&p).unwrap();
        assert_eq!(t.lines().next().unwrap(), HEADER);
        assert_eq!(t.lines().count(), 3);
        assert_eq!(read(&p).unwrap().len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(p.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            // never appends through a symlink
            let q = d.join("botport/link.csv");
            std::os::unix::fs::symlink(&p, &q).unwrap();
            assert!(append(&q, &row("2000-01-03T05:02:00Z", "x", 1, None)).is_err());
        }
        assert_eq!(read(&d.join("none.csv")).unwrap(), vec![]);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn stats_group_and_filter() {
        let mut rows = vec![];
        for (i, ms) in [100u64, 200, 300, 400, 1000].iter().enumerate() {
            rows.push(row(
                &format!("2000-01-03T05:0{i}:00Z"),
                "mirror-sync",
                *ms,
                Some(10 * (i as u64 + 1)),
            ));
        }
        rows.push(row("2000-01-01T00:00:00Z", "digest-write", 5, None));
        rows.push(row("garbage", "digest-write", 7, None));
        let g = stats(&rows, None);
        assert_eq!(g.len(), 2);
        let s = g.iter().find(|g| g.cmd == "mirror-sync").unwrap();
        assert_eq!((s.runs, s.p50_ms, s.p95_ms), (5, 300, 1000));
        assert_eq!((s.p50_tokens, s.p95_tokens), (Some(30), Some(50)));
        let d = g.iter().find(|g| g.cmd == "digest-write").unwrap();
        assert_eq!((d.runs, d.p50_tokens), (2, None));
        let since = parse_utc_iso("2000-01-02T00:00:00Z").unwrap();
        let g = stats(&rows, Some(since));
        assert_eq!(g.len(), 1, "old and unreadable rows are left out");
        assert!(render(&g).contains("mirror-sync"));
        assert_eq!(parse_since("7d").unwrap(), 604800);
        for bad in [
            "7w",
            "",
            "d",
            "7é",
            "99999999999999999999d",
            "18446744073709551615d",
        ] {
            assert!(parse_since(bad).is_err(), "{bad:?}");
        }
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[4], 95), 4);
    }
}

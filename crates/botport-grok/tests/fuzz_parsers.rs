//! Property tests for the debug-port parsers: no panic on arbitrary input, and round trips where a format has a
//! writer. A deterministic xorshift generator (no new dependency): fixed seeds, so a failure reproduces exactly;
//! `BOTPORT_FUZZ_ITERS` raises the case count for a longer local run.
use botport_grok::lifecycle::{
    debug_flag_pids_in, parse_devtools_active_port, parse_hid_idle_s, PortFile,
};
use botport_grok::transport::cdp::{click_point, ws_target_path};
use botport_grok::transport::ws::{is_upgrade_status, valid_ws_path};
use serde_json::{json, Value};

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn bits(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.bits() % n.max(1) as u64) as usize
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
    /// A string biased toward the characters parsers care about: digits, separators, line ends, controls,
    /// multi-byte and the empty string.
    pub fn string(&mut self, max: usize) -> String {
        const ATOMS: &[&str] = &[
            "0",
            "1",
            "9",
            "65535",
            "65536",
            "-1",
            " ",
            "\t",
            "\r",
            "\n",
            "\r\n",
            "/",
            ":",
            "=",
            "\"",
            "{",
            "}",
            "devtools",
            "/devtools/page/",
            "/devtools/browser/",
            "ws://127.0.0.1:",
            "HTTP/1.1",
            "101",
            "HIDIdleTime",
            "--remote-debugging-port=0",
            "Grok Bot",
            "é",
            "\u{0}",
            "\u{7f}",
            "\u{feff}",
            "日本",
            "🦀",
            "+",
            "e9",
            "18446744073709551616",
            "a",
            "Z",
        ];
        let n = self.below(max + 1);
        let mut s = String::new();
        for _ in 0..n {
            if self.below(4) == 0 {
                s.push(char::from_u32(self.below(0x11000) as u32).unwrap_or('?'));
            } else {
                s.push_str(self.pick(ATOMS));
            }
        }
        s
    }
    /// Mutate a seed: insert, delete, duplicate or replace a span (on char boundaries).
    pub fn mutate(&mut self, seed: &str) -> String {
        let mut s: Vec<char> = seed.chars().collect();
        for _ in 0..=self.below(4) {
            let at = self.below(s.len() + 1);
            match self.below(4) {
                0 => {
                    let ins: Vec<char> = self.string(3).chars().collect();
                    s.splice(at..at, ins);
                }
                1 if at < s.len() => {
                    let end = (at + 1 + self.below(4)).min(s.len());
                    s.drain(at..end);
                }
                2 if at < s.len() => {
                    let end = (at + 1 + self.below(8)).min(s.len());
                    let dup: Vec<char> = s[at..end].to_vec();
                    s.splice(at..at, dup);
                }
                _ if at < s.len() => s[at] = self.string(1).chars().next().unwrap_or('x'),
                _ => {}
            }
        }
        s.into_iter().collect()
    }
}

fn iters(default: usize) -> usize {
    std::env::var("BOTPORT_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Random JSON values, nested a little.
fn value(r: &mut Rng, depth: u32) -> Value {
    match r.below(if depth == 0 { 6 } else { 8 }) {
        0 => Value::Null,
        1 => json!(r.below(2) == 0),
        2 => json!(r.bits() as i64),
        3 => {
            let f = f64::from_bits(r.bits());
            json!(if f.is_finite() { f } else { 1.5 })
        }
        4 => json!(r.string(6)),
        5 => json!(r.below(2000) as f64 - 1000.0),
        6 => Value::Array((0..r.below(4)).map(|_| value(r, depth - 1)).collect()),
        _ => {
            let keys = ["x", "y", "hit", "approval", "z"];
            let mut m = serde_json::Map::new();
            for _ in 0..r.below(6) {
                m.insert(r.pick(&keys).to_string(), value(r, depth - 1));
            }
            Value::Object(m)
        }
    }
}

// ------------------------------------------------------------------ DevToolsActivePort

#[test]
fn devtools_active_port_never_panics_and_accepts_only_a_port_line() {
    let seeds = [
        "9222\n/devtools/browser/abc\n",
        "0\n",
        "65535\r\n/devtools/browser/x",
        "",
    ];
    let mut r = Rng::new(1);
    for i in 0..iters(20_000) {
        let s = if i % 2 == 0 {
            r.string(12)
        } else {
            let seed = r_pick(&mut r, &seeds);
            r.mutate(seed)
        };
        if let Some((port, path)) = parse_devtools_active_port(&s) {
            let first = s.lines().next().unwrap().trim();
            assert_eq!(first.parse::<u16>().ok(), Some(port), "{s:?}");
            assert!(!path.contains('\n') && path == path.trim(), "{s:?}");
        }
    }
}

fn r_pick<'a>(r: &mut Rng, v: &[&'a str]) -> &'a str {
    v[r.below(v.len())]
}

#[test]
fn devtools_active_port_round_trips() {
    let mut r = Rng::new(2);
    for _ in 0..iters(5_000) {
        let port = r.bits() as u16;
        let id: String = (0..r.below(40))
            .map(|_| *r.pick(&['a', 'f', '0', '9', '-']))
            .collect();
        let path = format!("/devtools/browser/{id}");
        let nl = if r.below(2) == 0 { "\n" } else { "\r\n" };
        let tail = if r.below(2) == 0 { nl } else { "" };
        let text = format!("{port}{nl}{path}{tail}");
        assert_eq!(
            parse_devtools_active_port(&text),
            Some((port, path)),
            "{text:?}"
        );
    }
    assert_eq!(
        parse_devtools_active_port("9222"),
        Some((9222, String::new()))
    );
    assert_eq!(parse_devtools_active_port("70000\n/x"), None);
    assert_eq!(parse_devtools_active_port("\n9222"), None);
}

// ------------------------------------------------------------------ port file

#[test]
fn port_file_round_trips_and_expiry_never_overflows() {
    let mut r = Rng::new(3);
    for _ in 0..iters(5_000) {
        let pf = PortFile {
            port: r.bits() as u16,
            pid: r.bits() as u32,
            ws_path: r.string(8),
            opened_at_ms: r.bits(),
            ttl_s: r.bits(),
        };
        let s = serde_json::to_string(&pf).unwrap();
        assert_eq!(serde_json::from_str::<PortFile>(&s).unwrap(), pf);
        assert!(pf.expires_ms() >= pf.opened_at_ms);
        // a hand-edited file: arbitrary mutations either parse or are refused, never panic
        let m = r.mutate(&s);
        if let Ok(p) = serde_json::from_str::<PortFile>(&m) {
            let _ = p.expires_ms();
        }
    }
}

// ------------------------------------------------------------------ ps / ioreg / lsof-style text

#[test]
fn ps_and_ioreg_parsers_never_panic() {
    const EXE: &str = "/Applications/Grok Bot.app/Contents/MacOS/";
    let seeds = [
        "  101 /Applications/Grok Bot.app/Contents/MacOS/Grok Bot --remote-debugging-port=0\n",
        "102 /Applications/Grok Bot.app/Contents/MacOS/Grok Bot Helper --remote-debugging-port=0",
        "    | |   \"HIDIdleTime\" = 754000000000\n",
    ];
    let mut r = Rng::new(4);
    for i in 0..iters(20_000) {
        let s = if i % 3 == 0 {
            r.string(16)
        } else {
            let seed = r_pick(&mut r, &seeds);
            r.mutate(seed)
        };
        for pid in debug_flag_pids_in(&s, EXE) {
            assert!(s.contains(&pid.to_string()), "{s:?}");
        }
        // an exe dir that is itself odd (multi-byte, empty) must not split a char
        let _ = debug_flag_pids_in(&s, &r.string(3));
        if let Some(sec) = parse_hid_idle_s(&s) {
            assert!(sec <= u64::MAX / 1_000_000_000);
        }
    }
}

// ------------------------------------------------------------------ websocket / target URLs

#[test]
fn ws_path_and_target_checks_never_panic_and_agree() {
    let prefix = "ws://127.0.0.1:9222";
    let seeds = [
        "ws://127.0.0.1:9222/devtools/page/ABCDEF",
        "ws://127.0.0.1:92220/devtools/page/x",
        "ws://127.0.0.1:9222/devtools/browser/y",
        "HTTP/1.1 101 Switching Protocols\r\n",
    ];
    let mut r = Rng::new(5);
    for i in 0..iters(20_000) {
        let s = if i % 2 == 0 {
            r.string(10)
        } else {
            let seed = r_pick(&mut r, &seeds);
            r.mutate(seed)
        };
        if valid_ws_path(&s) {
            assert!(s.bytes().all(|b| b.is_ascii_graphic()), "{s:?}");
        }
        if let Ok(p) = ws_target_path(&s, prefix) {
            assert!(
                p.starts_with("/devtools/page/") && valid_ws_path(p),
                "{s:?}"
            );
            assert_eq!(format!("{prefix}{p}"), s);
        }
        if is_upgrade_status(&s) {
            assert!(
                s.starts_with("HTTP/1.1 101") || s.starts_with("HTTP/1.0 101"),
                "{s:?}"
            );
        }
    }
}

#[test]
fn click_point_never_panics_and_only_clicks_a_confirmed_hit() {
    let mut r = Rng::new(6);
    for _ in 0..iters(20_000) {
        let v = value(&mut r, 2);
        if let Ok(Some((x, y))) = click_point(&v) {
            assert_eq!(v["hit"], json!(true));
            assert_eq!(v["approval"], json!(false));
            assert_eq!(v["x"].as_f64(), Some(x));
            assert_eq!(v["y"].as_f64(), Some(y));
        }
    }
}

//! Property tests for the grok-bot-export reader (`adapters::grok_bot::export_json`): no panic on arbitrary or
//! mutated input (strict, tolerant and file paths), and round trips: a document split across fenced code blocks,
//! wrapped in chatter, or given trailing commas reads back as the same value. A deterministic xorshift
//! generator (no new dependency): fixed seeds reproduce; `BOTPORT_FUZZ_ITERS` raises the case count.
use botport::adapters::grok_bot::export_json::{self as ej, document, parse};
use serde_json::Value;
use std::path::{Path, PathBuf};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn bits(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.bits() % n.max(1) as u64) as usize
    }
    /// Text biased toward what the tolerant reader looks at: fences, braces, quotes (straight and curly),
    /// commas, escapes, line ends, counted keys and multi-byte characters.
    fn string(&mut self, max: usize) -> String {
        const ATOMS: &[&str] = &[
            "```",
            "```json\n",
            "~~~",
            "````",
            "\n```\n",
            "{",
            "}",
            "[",
            "]",
            ",",
            ", ",
            ":",
            "\"",
            "\\",
            "\\\"",
            "“",
            "”",
            "‘",
            "’",
            "\n",
            "\r\n",
            " ",
            "\"memories\"",
            "\"skills\"",
            "\"bots\"",
            "\"format\"",
            "\"grok-bot-export\"",
            "\"routines\"",
            "\"token\"",
            "1",
            "-0.5e9",
            "null",
            "true",
            "é",
            "🦀",
            "\u{0}",
            "Here is the export:",
            "x",
        ];
        let n = self.below(max + 1);
        let mut s = String::new();
        for _ in 0..n {
            if self.below(5) == 0 {
                s.push(char::from_u32(self.below(0x11000) as u32).unwrap_or('?'));
            } else {
                s.push_str(ATOMS[self.below(ATOMS.len())]);
            }
        }
        s
    }
    fn mutate(&mut self, seed: &str) -> String {
        let mut s: Vec<char> = seed.chars().collect();
        for _ in 0..=self.below(5) {
            let at = self.below(s.len() + 1);
            match self.below(4) {
                0 => {
                    let ins: Vec<char> = self.string(2).chars().collect();
                    s.splice(at..at, ins);
                }
                1 if at < s.len() => {
                    let end = (at + 1 + self.below(6)).min(s.len());
                    s.drain(at..end);
                }
                2 if at < s.len() => {
                    let end = (at + 1 + self.below(30)).min(s.len());
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

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn seeds() -> Vec<String> {
    let mut v =
        vec![std::fs::read_to_string(fixtures().join("grok-export/grok-bot-export.json")).unwrap()];
    let mut cases: Vec<PathBuf> = std::fs::read_dir(fixtures().join("paste-cases"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    cases.sort();
    v.extend(cases.iter().map(|p| std::fs::read_to_string(p).unwrap()));
    v
}

fn export() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("grok-export/grok-bot-export.json")).unwrap(),
    )
    .unwrap()
}

/// The parsed document, when the reader accepts something, is a JSON value it can re-serialise and re-read.
fn check_parse(s: &str) {
    if let Ok((v, _)) = parse(s) {
        let again = serde_json::to_string(&v).unwrap();
        assert_eq!(
            parse(&again).unwrap().0,
            v,
            "re-read of an accepted document: {s:?}"
        );
    }
    let _ = document(s);
}

#[test]
fn arbitrary_text_never_panics() {
    let mut r = Rng::new(11);
    for _ in 0..iters(20_000) {
        check_parse(&r.string(24));
    }
}

#[test]
fn mutated_pastes_never_panic() {
    let seeds = seeds();
    let mut r = Rng::new(12);
    for _ in 0..iters(6_000) {
        let seed = &seeds[r.below(seeds.len())];
        check_parse(&r.mutate(seed));
    }
}

/// The file entry points (bots, read_model, load_bundle, truncated_count) on mutated exports: an error or a
/// model, never a panic (e.g. an empty `bots` list, a non-string name, a non-array memories field).
#[test]
fn file_readers_never_panic_on_mutated_exports() {
    let dir = std::env::temp_dir().join(format!("botport-fuzz-export-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("export.json");
    let base =
        std::fs::read_to_string(fixtures().join("grok-export/grok-bot-export.json")).unwrap();
    let mut r = Rng::new(13);
    for _ in 0..iters(1_500) {
        std::fs::write(&p, r.mutate(&base)).unwrap();
        let _ = ej::bots(&p);
        let _ = ej::read_model(&p);
        let _ = ej::load_bundle(&p);
        let _ = ej::truncated_count(&p);
        let _ = ej::is_export(&p);
    }
    // structural variants of a valid document
    let mut v = export();
    for bots in [
        serde_json::json!([]),
        serde_json::json!([{}]),
        serde_json::json!([{"name": 7}]),
        serde_json::json!([{"name": "N", "memories": "x", "skills": {}, "routines": [1, null, "s"]}]),
        serde_json::json!("bots"),
    ] {
        v["bots"] = bots;
        std::fs::write(&p, v.to_string()).unwrap();
        let _ = ej::read_model(&p);
        let _ = ej::load_bundle(&p);
    }
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn slug_and_text_never_panic_and_slug_is_well_formed() {
    let mut r = Rng::new(14);
    for _ in 0..iters(20_000) {
        let s = r.string(12);
        let g = ej::slug(&s);
        assert!(!g.is_empty() && g.chars().count() <= 64, "{s:?} -> {g:?}");
        assert!(!g.starts_with('-') && !g.ends_with('-'), "{s:?} -> {g:?}");
        assert!(
            g.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{s:?} -> {g:?}"
        );
        assert_eq!(ej::slug(&g), g, "slug is idempotent");
        let a = ej::app_id(&s);
        assert!(a.chars().count() <= 32, "{s:?} -> {a:?}");
        let _ = ej::text(&Value::String(s));
    }
}

/// A compact export cut into 2..6 pieces, each in its own fenced block with chatter between (a reply split
/// across chat messages), reads back as the same document.
#[test]
fn split_fenced_export_round_trips() {
    let v = export();
    let compact = serde_json::to_string(&v).unwrap();
    let chars: Vec<char> = compact.chars().collect();
    let mut r = Rng::new(15);
    for _ in 0..iters(2_000) {
        let n = 2 + r.below(5);
        let mut cuts: Vec<usize> = (0..n - 1).map(|_| 1 + r.below(chars.len() - 1)).collect();
        cuts.sort();
        cuts.dedup();
        let mut parts = vec![];
        let mut at = 0;
        for c in cuts.iter().copied().chain([chars.len()]) {
            parts.push(chars[at..c].iter().collect::<String>());
            at = c;
        }
        let fence = ["```", "```json", "~~~", "````json"][r.below(4)];
        let close = &fence[..fence.find('j').unwrap_or(fence.len())];
        let nl = if r.below(2) == 0 { "\n" } else { "\r\n" };
        let mut paste = String::from("Here is part one of the export:\n");
        for (i, p) in parts.iter().enumerate() {
            paste.push_str(&format!(
                "{fence}{nl}{p}{nl}{close}{nl}(part {} of {})\n\n",
                i + 1,
                parts.len()
            ));
        }
        let (got, repairs) = parse(&paste).unwrap_or_else(|e| panic!("{e}\n{paste}"));
        assert_eq!(got, v, "{paste}");
        assert!(!repairs.is_empty());
    }
}

/// Chatter around the document and trailing commas before `}` / `]` are repaired, back to the same value.
#[test]
fn chatter_and_trailing_commas_round_trip() {
    let v = export();
    let pretty = serde_json::to_string_pretty(&v).unwrap();
    let mut r = Rng::new(16);
    for _ in 0..iters(2_000) {
        // add a trailing comma before a random subset of closing brackets outside strings
        let mut out = String::new();
        let (mut in_str, mut esc) = (false, false);
        for c in pretty.chars() {
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
            } else if c == '"' {
                in_str = true;
            } else if (c == '}' || c == ']') && r.below(3) == 0 {
                let t = out.trim_end();
                if !t.ends_with('{') && !t.ends_with('[') {
                    out.truncate(t.len());
                    out.push(',');
                    out.push('\n');
                }
            }
            out.push(c);
        }
        let wrapped = if r.below(2) == 0 {
            format!("Sure! Here it is:\n{out}\nLet me know if you need anything else.")
        } else {
            out
        };
        let (got, _) = parse(&wrapped).unwrap_or_else(|e| panic!("{e}\n{wrapped}"));
        assert_eq!(got, v);
        document(&wrapped).unwrap();
    }
}

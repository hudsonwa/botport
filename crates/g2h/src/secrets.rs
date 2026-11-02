//! Outbound secret scanning and redaction.
//!
//! Pattern rules catch common key shapes. `.env` fingerprints hold only
//! sha256 hashes of values (plus their byte length); text is matched by a
//! sliding window of each fingerprinted length, so values are never stored.

use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

pub const REDACTED: &str = "[REDACTED]";
/// Shorter `.env` values are ignored (too many false positives).
pub const MIN_FINGERPRINT_LEN: usize = 8;

fn patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        [
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            r"\bsk-[A-Za-z0-9_\-]{16,}",
            r"\bxai-[A-Za-z0-9_\-]{16,}",
            r"\bgh[pousr]_[A-Za-z0-9]{20,}",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"(?i)\bbearer\s+[A-Za-z0-9._~+/\-]{16,}=*",
            // KEY/TOKEN/SECRET/PASSWORD = long token: redact the value only.
            r#"(?i)(?:key|token|secret|passw(?:or)?d)["']?\s*[:=]\s*["']?([A-Za-z0-9+/_\-]{20,}=*)"#,
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid secret regex"))
        .collect()
    })
}

/// Parse dotenv text into (key, value) pairs. Supports `export `, single or
/// double quoted values (kept verbatim, `#` included) and inline ` # comment`
/// after unquoted values (`#` not preceded by whitespace stays in the value).
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim_start();
        let value = match v.chars().next() {
            Some(q @ ('"' | '\'')) => match v[1..].find(q) {
                Some(end) => v[1..1 + end].to_string(),
                None => v.to_string(),
            },
            _ => {
                let mut end = v.len();
                let b = v.as_bytes();
                for i in 0..b.len() {
                    if b[i] == b'#' && i > 0 && b[i - 1].is_ascii_whitespace() {
                        end = i;
                        break;
                    }
                }
                v[..end].trim_end().to_string()
            }
        };
        out.push((k.trim().to_string(), value));
    }
    out
}

#[derive(Debug, Default, Clone)]
pub struct Fingerprints {
    /// byte length -> set of sha256 digests
    by_len: BTreeMap<usize, BTreeSet<[u8; 32]>>,
}

fn digest(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

impl Fingerprints {
    pub fn add_value(&mut self, value: &str) {
        if value.len() >= MIN_FINGERPRINT_LEN {
            self.by_len
                .entry(value.len())
                .or_default()
                .insert(digest(value.as_bytes()));
        }
    }

    /// Fingerprint every value in a `KEY=value` dotenv text.
    pub fn add_env_text(&mut self, text: &str) {
        for (_, v) in parse_dotenv(text) {
            self.add_value(&v);
        }
    }

    pub fn add_env_file(&mut self, path: &Path) -> std::io::Result<()> {
        let text = std::fs::read_to_string(path)?;
        self.add_env_text(&text);
        Ok(())
    }

    pub fn merge(&mut self, other: Fingerprints) {
        for (len, set) in other.by_len {
            self.by_len.entry(len).or_default().extend(set);
        }
    }

    pub fn len(&self) -> usize {
        self.by_len.values().map(|s| s.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Byte ranges of fingerprinted values found in `text`.
    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let b = text.as_bytes();
        let mut out = Vec::new();
        for (&len, set) in &self.by_len {
            if len > b.len() {
                continue;
            }
            for i in 0..=b.len() - len {
                if text.is_char_boundary(i)
                    && text.is_char_boundary(i + len)
                    && set.contains(&digest(&b[i..i + len]))
                {
                    out.push((i, i + len));
                }
            }
        }
        out
    }
}

/// Redact secrets; returns (redacted text, number of redactions).
pub fn redact(text: &str, fp: Option<&Fingerprints>) -> (String, usize) {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(fp) = fp {
        ranges.extend(fp.find(text));
    }
    for re in patterns() {
        for c in re.captures_iter(text) {
            let m = c.get(1).unwrap_or_else(|| c.get(0).unwrap());
            ranges.push((m.start(), m.end()));
        }
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for (s, e) in &merged {
        out.push_str(&text[pos..*s]);
        out.push_str(REDACTED);
        pos = *e;
    }
    out.push_str(&text[pos..]);
    (out, merged.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test fixtures are synthetic, built at runtime so no key-shaped literal sits in the source.
    fn fake(prefix: &str, n: usize) -> String {
        format!(
            "{prefix}{}",
            "Ab3".repeat(n).chars().take(n).collect::<String>()
        )
    }

    #[test]
    fn each_key_shape_redacted() {
        let pem = format!(
            "-----BEGIN RSA {}-----\nMIIabc\n-----END RSA {}-----",
            "PRIVATE KEY", "PRIVATE KEY"
        );
        let cases = vec![
            fake("sk-", 32),
            fake("xai-", 40),
            fake("ghp_", 36),
            format!("AKIA{}", "ABCDEFGHIJKLMNOP"),
            format!("Bearer {}", fake("", 30)),
            pem,
        ];
        for s in cases {
            let (r, n) = redact(&format!("see {s} ok"), None);
            assert_eq!(n, 1, "{s}");
            assert_eq!(r, format!("see {REDACTED} ok"), "{s}");
        }
    }

    #[test]
    fn assigned_tokens_redact_value_only() {
        for k in ["API_KEY", "token", "SECRET", "db_password"] {
            let (r, n) = redact(&format!("{k}={}", fake("", 32)), None);
            assert_eq!((r, n), (format!("{k}={REDACTED}"), 1));
        }
        let (r, n) = redact("password: short", None);
        assert_eq!((r.as_str(), n), ("password: short", 0));
    }

    #[test]
    fn env_fingerprint_redacts_without_storing_value() {
        let val = "q7-plain-looking-value";
        let mut fp = Fingerprints::default();
        fp.add_env_text(&format!("# c\nexport MY_THING=\"{val}\"\nSHORT=abc\n"));
        assert_eq!(fp.len(), 1);
        assert!(!format!("{fp:?}").contains(val));
        let (r, n) = redact(&format!("é the value is {val}!"), Some(&fp));
        assert_eq!((r, n), (format!("é the value is {REDACTED}!"), 1));
    }

    #[test]
    fn env_file_load() {
        let p = std::env::temp_dir().join(format!("g2h-env-{}", std::process::id()));
        std::fs::write(&p, "X='single-quoted-v'\n").unwrap();
        let mut fp = Fingerprints::default();
        fp.add_env_file(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(redact("a single-quoted-v b", Some(&fp)).1, 1);
    }

    #[test]
    fn clean_text_untouched() {
        let t = "hello there, the sky is blue";
        assert_eq!(redact(t, None), (t.to_string(), 0));
    }

    #[test]
    fn dotenv_inline_comments_and_quotes() {
        let kv = parse_dotenv(
            "A=plain-value-1 # trailing\nB=\"quoted # kept\" # c\nC='single # kept'\nD=has#hash\n# x=y\nexport E = spaced-val \n",
        );
        let get = |k: &str| kv.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
        assert_eq!(get("A"), Some("plain-value-1"));
        assert_eq!(get("B"), Some("quoted # kept"));
        assert_eq!(get("C"), Some("single # kept"));
        assert_eq!(get("D"), Some("has#hash"));
        assert_eq!(get("E"), Some("spaced-val"));
        assert_eq!(get("x"), None);
        let mut fp = Fingerprints::default();
        fp.add_env_text("K=abcdefgh-123 # note\n");
        assert_eq!(redact("v abcdefgh-123 w", Some(&fp)).1, 1);
    }
}

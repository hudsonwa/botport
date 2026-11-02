//! YAML 1.1 compatibility. Python consumers read config.yaml with PyYAML (YAML 1.1); serde_yaml speaks YAML 1.2.
//! - `emit`: our own emitter instead of serde_yaml's: every string is double-quoted with JSON escapes (valid YAML
//!   1.1 and 1.2, so PyYAML can never read "off" as false or "2026-10-30" as a date), floats always carry a dot and
//!   a signed exponent, and a key stays plain only when no YAML 1.1 resolver would type it.
//! - `read`: in hand-written files, plain `off`/`yes`/`on`/`n`… are booleans to PyYAML; they become true/false
//!   before serde_yaml parses, so comparisons mean what the reader means. A line scanner that never touches block
//!   scalars, multi-line quoted scalars, plain-scalar continuations or explicit (`?`) keys and their values;
//!   flow mappings and anchored values are left alone (documented gap: they stay strings).
use regex::Regex;
use serde_yaml::Value as Y;
use std::sync::OnceLock;

fn bool11(s: &str) -> Option<bool> {
    match s {
        "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" | "true" | "True" | "TRUE" => {
            Some(true)
        }
        "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" | "false" | "False" | "FALSE" => {
            Some(false)
        }
        _ => None,
    }
}

/// Would PyYAML's implicit resolvers give this plain scalar a non-string type (or fail on it)?
fn typed11(s: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| {
        Regex::new(concat!(
            r"^(?:",
            r"[-+]?(?:0b[01_]+|0[0-7_]+|(?:0|[1-9][0-9_]*)|0x[0-9a-fA-F_]+|[1-9][0-9_]*(?::[0-5]?[0-9])+)", // int
            r"|[-+]?(?:[0-9][0-9_]*)?\.[0-9.]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*", // float
            r"|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
            r"|~|null|Null|NULL|=|<<",
            r"|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}.*", // timestamp
            r")$"
        ))
        .unwrap()
    });
    s.is_empty() || bool11(s).is_some() || r.is_match(s)
}

/// End (exclusive) of a quoted scalar starting at s[0] (a quote char), or None if it does not close on this line.
fn quoted_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let q = b[0];
    let mut i = 1;
    while i < b.len() {
        if q == b'"' && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Does a quoted scalar that opened on an earlier line close on this one?
fn closes(s: &str, q: char) -> bool {
    quoted_end(&format!("{q}{s}")).is_some()
}

struct Line {
    /// indentation of the node that owns this line's value: the key column, else the last `-` column, else the indent
    node: usize,
    explicit: bool,
    key: Option<(usize, usize)>,
    val: (usize, usize),
}

/// Split `  - - key: value  # c` into key and value spans (byte offsets). None for lines it does not understand.
fn parse(body: &str) -> Option<Line> {
    let b = body.as_bytes();
    let mut i = body.len() - body.trim_start_matches(' ').len();
    let mut node = i;
    while i < b.len() && b[i] == b'-' && (i + 1 == b.len() || b[i + 1] == b' ') {
        node = i;
        i += 1;
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
    }
    let col = i;
    let rest = &body[i..];
    if rest.is_empty() || rest.starts_with('#') {
        return None;
    }
    if rest == "?" || rest.starts_with("? ") || rest == ":" || rest.starts_with(": ") {
        return Some(Line {
            node,
            explicit: true,
            key: None,
            val: (i, i),
        });
    }
    // key
    let mut key = None;
    let mut j = i;
    if rest.starts_with(['"', '\'']) {
        if let Some(e) = quoted_end(rest) {
            let after = &rest[e..];
            if after.starts_with(':') && (after.len() == 1 || after[1..].starts_with(' ')) {
                key = Some((i, i + e));
                j = i + e + 1;
            }
        }
    } else if !rest.starts_with(['[', '{', '&', '*', '!', '|', '>', '%', '@', '`']) {
        let stop = rest.find(" #").unwrap_or(rest.len());
        let k = rest[..stop]
            .find(": ")
            .or_else(|| rest[..stop].ends_with(':').then(|| stop - 1));
        if let Some(k) = k {
            key = Some((i, i + k));
            j = i + k + 1;
        }
    }
    while j < b.len() && b[j] == b' ' {
        j += 1;
    }
    // value
    let v = &body[j..];
    let ve = if v.starts_with(['"', '\'']) {
        quoted_end(v).map(|e| j + e).unwrap_or(body.len())
    } else {
        let stop = v.find(" #").unwrap_or(v.len());
        j + v[..stop].trim_end().len()
    };
    Some(Line {
        node: if key.is_some() { col } else { node },
        explicit: false,
        key,
        val: (j, ve),
    })
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

#[derive(PartialEq)]
enum Mode {
    Normal,
    /// content lines are indented more than this column (block scalar or plain multi-line continuation)
    Deeper(usize),
    Quoted(char),
}

/// Rewrite keys with `fk` and plain scalar values with `fv` (also the items of one-line flow sequences).
fn map_lines(
    text: &str,
    fk: &dyn Fn(&str) -> Option<String>,
    fv: &dyn Fn(&str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(text.len() + 256);
    let mut mode = Mode::Normal;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        match mode {
            Mode::Deeper(c) if body.trim().is_empty() || indent(body) > c => {
                out += line;
                continue;
            }
            Mode::Quoted(q) => {
                out += line;
                if closes(body, q) {
                    mode = Mode::Normal;
                }
                continue;
            }
            _ => mode = Mode::Normal,
        }
        let t = body.trim_start();
        let Some(l) = (if t.starts_with('#') || t == "---" || t == "..." {
            None
        } else {
            parse(body)
        }) else {
            out += line;
            continue;
        };
        if l.explicit {
            // `? key` / `: value` of a complex key: leave it and everything nested under it untouched
            mode = Mode::Deeper(l.node);
            out += line;
            continue;
        }
        let val = &body[l.val.0..l.val.1];
        let new_key = l.key.and_then(|(s, e)| fk(&body[s..e]).map(|k| (s, e, k)));
        let new_val: Option<String> = if val.starts_with('|') || val.starts_with('>') {
            mode = Mode::Deeper(l.node);
            None
        } else if let Some(q) = val.chars().next().filter(|c| *c == '"' || *c == '\'') {
            if quoted_end(val).is_none() {
                mode = Mode::Quoted(q);
            }
            None
        } else if val.starts_with('[') && val.ends_with(']') {
            if val[1..val.len() - 1].contains(['{', '[', '"', '\'']) {
                None
            } else {
                let items: Vec<String> = val[1..val.len() - 1]
                    .split(',')
                    .map(|it| {
                        let tr = it.trim();
                        match fv(tr) {
                            Some(n) if !tr.is_empty() => it.replacen(tr, &n, 1),
                            _ => it.to_string(),
                        }
                    })
                    .collect();
                Some(format!("[{}]", items.join(",")))
            }
        } else if val.is_empty() || val.starts_with(['{', '&', '*', '!']) {
            None
        } else {
            mode = Mode::Deeper(l.node); // a plain scalar may continue on more-indented lines
            fv(val)
        };
        if new_key.is_none() && new_val.is_none() {
            out += line;
            continue;
        }
        let mut s = body.to_string();
        // replace the value first (it lies after the key), then the key
        if let Some(v) = new_val {
            s.replace_range(l.val.0..l.val.1, &v);
        }
        if let Some((ks, ke, k)) = new_key {
            s.replace_range(ks..ke, &k);
        }
        out += &s;
        out += &line[body.len()..];
    }
    out
}

/// YAML text with YAML-1.1 plain booleans spelled true/false (parse the result with serde_yaml).
pub fn read(text: &str) -> String {
    map_lines(text, &|_| None, &|v| bool11(v).map(|b| b.to_string()))
}

fn emit_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            ".inf".into()
        } else {
            "-.inf".into()
        };
    }
    let t = format!("{f:?}"); // shortest round-trip form: "1.5", "1e20", "1.5e-7"
    let (m, e) = match t.split_once('e') {
        Some((m, e)) => (m.to_string(), Some(e.to_string())),
        None => (t, None),
    };
    let m = if m.contains('.') { m } else { format!("{m}.0") };
    match e {
        Some(e) if e.starts_with('-') => format!("{m}e{e}"),
        Some(e) => format!("{m}e+{e}"),
        None => m,
    }
}

/// Double-quoted YAML string; everything outside PyYAML's printable set (and line-break lookalikes) escaped.
fn dq(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                o.push_str(&format!("\\x{:02x}", c as u32))
            }
            '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}' => {
                o.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn emit_scalar(v: &Y) -> Option<String> {
    Some(match v {
        Y::Null => "null".into(),
        Y::Bool(b) => b.to_string(),
        Y::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) => emit_float(f),
            _ => n.to_string(),
        },
        Y::String(s) => dq(s),
        Y::Mapping(m) if m.is_empty() => "{}".into(),
        Y::Sequence(q) if q.is_empty() => "[]".into(),
        Y::Tagged(t) => return emit_scalar(&t.value),
        _ => return None,
    })
}

fn emit_key(k: &Y) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let plain = R.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_./-]*$").unwrap());
    match k {
        Y::String(s) if plain.is_match(s) && !typed11(s) => s.clone(),
        other => emit_scalar(other).unwrap_or_else(|| {
            serde_json::to_string(&serde_yaml::to_string(other).unwrap_or_default()).unwrap()
        }),
    }
}

fn emit_node(v: &Y, ind: usize, out: &mut String) {
    let pad = " ".repeat(ind);
    match v {
        Y::Mapping(m) if !m.is_empty() => {
            for (k, x) in m {
                out.push_str(&pad);
                out.push_str(&emit_key(k));
                out.push(':');
                match emit_scalar(x) {
                    Some(s) => {
                        out.push(' ');
                        out.push_str(&s);
                        out.push('\n');
                    }
                    None => {
                        out.push('\n');
                        emit_node(x, ind + 2, out);
                    }
                }
            }
        }
        Y::Sequence(q) if !q.is_empty() => {
            for x in q {
                out.push_str(&pad);
                out.push('-');
                match emit_scalar(x) {
                    Some(s) => {
                        out.push(' ');
                        out.push_str(&s);
                        out.push('\n');
                    }
                    None => {
                        // nested collection: first line after "- ", the rest indented under it
                        let mut sub = String::new();
                        emit_node(x, ind + 2, &mut sub);
                        out.push(' ');
                        out.push_str(&sub[ind + 2..]);
                    }
                }
            }
        }
        Y::Tagged(t) => emit_node(&t.value, ind, out),
        other => {
            out.push_str(&pad);
            out.push_str(&emit_scalar(other).unwrap_or_default());
            out.push('\n');
        }
    }
}

/// YAML for `v` that PyYAML (1.1) and serde_yaml (1.2) both read back as exactly `v`.
pub fn emit(v: &Y) -> String {
    let mut out = String::new();
    emit_node(v, 0, &mut out);
    out
}

/// `emit`, refusing output that does not parse back to the same value (never write a config the reader can't load).
pub fn emit_checked(v: &Y) -> Result<String, String> {
    let out = emit(v);
    match serde_yaml::from_str::<Y>(&out) {
        Ok(back) if back == *v => Ok(out),
        _ => Err("config.yaml re-emit does not round-trip; nothing written".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn y(t: &str) -> Y {
        serde_yaml::from_str(t).unwrap()
    }

    #[test]
    fn emit_roundtrips_and_quotes_every_string() {
        for t in ["a: 'off'\nb: false\nc: 'see #1'\ne: 3\nf: '2026-10-30'\ng: '1_000'\nh: '='\ni: x\n",
                  "- \"a\\nb\"\n- k: [\"x\\noff: y\"]\n  'off': 1\n  'yes': '<<'\n- []\n- {}\n- - 1\n  - 2.5\n",
                  "? \"a\\nb\"\n: [off]\nlong: 1.0e20\nneg: -1.5e-7\nn: ~\n"] {
            let v = y(t);
            let e = emit(&v);
            assert_eq!(y(&e), v, "{e}");
        }
        let e = emit(&y("a: 'off'\n'on': 1\nExamples-V1.0-Model-0001: none\n"));
        assert_eq!(
            e,
            "a: \"off\"\n\"on\": 1\nExamples-V1.0-Model-0001: \"none\"\n"
        );
        assert_eq!(emit_float(1e20), "1.0e+20");
        assert_eq!(emit_float(2.0), "2.0");
        let odd = Y::String("a\u{7f}b\u{85}c\u{2028}d\u{feff}e\u{9f}\"\\\t".into());
        let e = emit(&odd);
        assert!(e.is_ascii(), "{e}");
        assert_eq!(y(&e), odd);
    }

    #[test]
    fn read_makes_booleans_like_pyyaml_and_respects_quotes() {
        let t = "a: off\nb: 'off'\nc: [x, Yes]\nd: \"q # off\"\ne: off # comment\nf: foo\n  off\ng:\n- no\n";
        let r = read(t);
        assert_eq!(r, "a: false\nb: 'off'\nc: [x, true]\nd: \"q # off\"\ne: false # comment\nf: foo\n  off\ng:\n- false\n");
    }

    #[test]
    fn read_never_touches_blocks_in_sequences_or_explicit_keys() {
        for t in [
            "k:\n- |\n  off\n  yes: no\n",
            "- >\n  off\n",
            "- - |\n    off\n",
            "? off\n: on\nz: off\n",
        ] {
            let r = read(t);
            assert_eq!(r.replace("z: false", "z: off"), t, "{r}");
        }
    }

    #[test]
    fn typed11_matches_pyyaml_resolvers() {
        for s in [
            "1_000",
            "+1_0",
            "0x1F",
            "017",
            "1:20",
            "1.5",
            ".inf",
            "~",
            "null",
            "=",
            "<<",
            "2026-10-30",
            "off",
            "",
        ] {
            assert!(typed11(s), "{s}");
        }
        for s in ["example-model", "8:00x", "a=b", "v1.2.3x"] {
            assert!(!typed11(s), "{s}");
        }
    }
}

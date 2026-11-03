//! YAML 1.1 compatibility. Python consumers read config.yaml with PyYAML (YAML 1.1); serde_yaml speaks YAML 1.2.
//! - `emit`: our own emitter instead of serde_yaml's: every string is double-quoted with JSON escapes (valid YAML
//!   1.1 and 1.2, so PyYAML can never read "off" as false or "2026-10-30" as a date), floats always carry a dot and
//!   a signed exponent, and a key stays plain only when no YAML 1.1 resolver would type it.
//! - `read`: in hand-written files, plain `off`/`yes`/`on`/`n`… are booleans to PyYAML; they become true/false
//!   before serde_yaml parses, so comparisons mean what the reader means. A line scanner that never touches block
//!   scalars, multi-line quoted scalars, plain-scalar continuations or explicit (`?`) keys and their values;
//!   flow mappings and anchored values are left alone (documented gap: they stay strings).
//! - `parse`: the one way botport parses YAML: size and nesting limits first, then serde_yaml.
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
pub fn typed11(s: &str) -> bool {
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
fn parse_line(body: &str) -> Option<Line> {
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
            parse_line(body)
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

/// Largest YAML document botport parses (profile configs, cron files and skill front matter are a few KiB).
pub const MAX_YAML_BYTES: usize = 4 * 1024 * 1024;
/// Deepest nesting botport parses (block indentation levels, `- ` sequence entries and `[`/`{` flow collections).
pub const MAX_YAML_DEPTH: usize = 64;

/// Parse a YAML document into `T`: the only place botport hands YAML to serde_yaml. The text is checked against
/// [`MAX_YAML_BYTES`] and [`MAX_YAML_DEPTH`] first, and then read once as a [`Node`] tree under the node cap
/// ([`max_nodes`]), so an oversized, deeply nested or alias-expanding document (a hostile skill's front matter, a
/// planted config) is refused before it is read into `T`.
#[allow(clippy::disallowed_methods)] // the one allowed call; clippy.toml refuses it everywhere else
pub fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    parse_all_nodes(text)?;
    // position only: a type error quotes the scalar it could not read, which may be a secret
    serde_yaml::from_str(text).map_err(|e| position_only(&e))
}

/// Every document of a YAML stream (`---` separated), each as a value, under the same limits as
/// [`parse_all_nodes`]. An empty stream reads as at most one null document. A syntax error is reported by its
/// position only ("not valid YAML at line L, column C"), never with the parser's message, which can quote the
/// document.
#[allow(clippy::disallowed_methods)] // the multi-document reader lives here with `parse`
pub fn parse_all(text: &str) -> Result<Vec<Y>, String> {
    use serde::Deserialize as _;
    parse_all_nodes(text)?;
    serde_yaml::Deserializer::from_str(text)
        .map(|doc| Y::deserialize(doc).map_err(|e| position_only(&e)))
        .collect()
}

/// A YAML node as read, keeping everything a value can hold: every mapping entry in order (duplicate keys
/// included, so none is dropped), any key (not only strings), and every tag with its text.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    /// a number, as written by the parser (`12`, `1.5`)
    Number(String),
    Str(String),
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
    /// a tagged node: the tag's text and the value
    Tagged(String, Box<Node>),
}

/// The most nodes any document may hold, aliases replayed included: far above a real config, skill or front matter
/// (a few hundred to a few thousand), far below what a 4 MiB file could otherwise expand to.
pub const MAX_YAML_NODES: usize = 100_000;

/// The most nodes a document may expand to (aliases replay the nodes they name): two per byte of text plus a
/// thousand, above any real file (a scalar and its separator take at least two bytes), and never more than
/// [`MAX_YAML_NODES`].
pub fn max_nodes(text: &str) -> usize {
    text.len()
        .saturating_mul(2)
        .saturating_add(1_000)
        .min(MAX_YAML_NODES)
}

/// The most string text (scalars, keys and tags, in bytes) a document may expand to: four times its length plus
/// 64 KiB, and never more than [`MAX_YAML_BYTES`], so an alias replaying a long string cannot ask for more.
pub fn max_text(text: &str) -> usize {
    text.len()
        .saturating_mul(4)
        .saturating_add(64 * 1024)
        .min(MAX_YAML_BYTES)
}

/// Every document of a YAML stream as [`Node`] trees, under [`check_limits`], the node cap and the text cap:
/// reading stops as soon as the expanded node count passes [`max_nodes`] or the expanded string text passes
/// [`max_text`]. Duplicate keys are kept, not refused. Errors give a position only.
#[allow(clippy::disallowed_methods)] // the node reader lives here with `parse`
pub fn parse_all_nodes(text: &str) -> Result<Vec<Node>, String> {
    use serde::de::DeserializeSeed as _;
    check_limits(text)?;
    let (count, bytes) = (std::cell::Cell::new(0usize), std::cell::Cell::new(0usize));
    let (cap, text_cap) = (max_nodes(text), max_text(text));
    serde_yaml::Deserializer::from_str(text)
        .map(|doc| {
            NodeSeed {
                count: &count,
                cap,
                bytes: &bytes,
                text_cap,
            }
            .deserialize(doc)
            .map_err(|e| {
                if count.get() > cap {
                    format!("YAML document expands to too many nodes (over {cap}; aliases?)")
                } else if bytes.get() > text_cap {
                    format!(
                        "YAML document expands to too much text (over {text_cap} bytes; aliases?)"
                    )
                } else {
                    position_only(&e)
                }
            })
        })
        .collect()
}

fn position_only(e: &serde_yaml::Error) -> String {
    match e.location() {
        Some(l) => format!("not valid YAML at line {}, column {}", l.line(), l.column()),
        None => "not valid YAML".into(),
    }
}

/// Reads one [`Node`], counting every node read and every byte of string text (aliases replayed included) and
/// stopping past either cap.
#[derive(Clone, Copy)]
struct NodeSeed<'a> {
    count: &'a std::cell::Cell<usize>,
    cap: usize,
    bytes: &'a std::cell::Cell<usize>,
    text_cap: usize,
}

impl NodeSeed<'_> {
    /// Adds `n` bytes of text to the total; an error once it passes the cap.
    fn text<E: serde::de::Error>(&self, n: usize) -> Result<(), E> {
        self.bytes.set(self.bytes.get().saturating_add(n));
        if self.bytes.get() > self.text_cap {
            return Err(E::custom("too much text"));
        }
        Ok(())
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for NodeSeed<'_> {
    type Value = Node;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Node, D::Error> {
        self.count.set(self.count.get() + 1);
        if self.count.get() > self.cap {
            return Err(serde::de::Error::custom("too many nodes"));
        }
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for NodeSeed<'_> {
    type Value = Node;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a YAML node")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Node, E> {
        Ok(Node::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Node, E> {
        Ok(Node::Number(v.to_string()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Node, E> {
        Ok(Node::Number(v.to_string()))
    }
    fn visit_i128<E>(self, v: i128) -> Result<Node, E> {
        Ok(Node::Number(v.to_string()))
    }
    fn visit_u128<E>(self, v: u128) -> Result<Node, E> {
        Ok(Node::Number(v.to_string()))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Node, E> {
        Ok(Node::Number(v.to_string()))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Node, E> {
        self.text(v.len())?;
        Ok(Node::Str(v.to_owned()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Node, E> {
        self.text(v.len())?;
        Ok(Node::Str(v))
    }
    fn visit_unit<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }
    fn visit_none<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Node, D::Error> {
        serde::de::DeserializeSeed::deserialize(self, d)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<Node, A::Error> {
        let mut out = vec![];
        while let Some(x) = a.next_element_seed(self)? {
            out.push(x);
        }
        Ok(Node::Seq(out))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut a: A) -> Result<Node, A::Error> {
        let mut out = vec![];
        while let Some(k) = a.next_key_seed(self)? {
            let v = a.next_value_seed(self)?;
            out.push((k, v));
        }
        Ok(Node::Map(out))
    }
    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, a: A) -> Result<Node, A::Error> {
        let (tag, v): (String, _) = a.variant()?;
        self.text::<A::Error>(tag.len())?;
        let inner = serde::de::VariantAccess::newtype_variant_seed(v, self)?;
        Ok(Node::Tagged(tag, Box::new(inner)))
    }
}

/// A YAML file's text, read with the size cap applied while reading: at most [`MAX_YAML_BYTES`] + 1 bytes are
/// ever read (and allocated); a longer file is refused before it is parsed.
pub fn read_yaml_file(p: &std::path::Path) -> Result<String, String> {
    let b =
        read_capped(p, MAX_YAML_BYTES as u64 + 1).map_err(|e| format!("{}: {e}", p.display()))?;
    if b.len() > MAX_YAML_BYTES {
        return Err(format!(
            "{}: YAML document too large (over {MAX_YAML_BYTES} bytes)",
            p.display()
        ));
    }
    String::from_utf8(b).map_err(|_| format!("{}: not UTF-8", p.display()))
}

/// The first [`MAX_YAML_BYTES`] bytes of a file as text (for front matter at its start): never more is read.
/// Missing, unreadable, not a regular file, or not UTF-8: empty (a multi-byte character cut by the cap is
/// dropped, not an error).
pub fn read_yaml_prefix(p: &std::path::Path) -> String {
    let Ok(mut b) = read_capped(p, MAX_YAML_BYTES as u64) else {
        return String::new();
    };
    match String::from_utf8(std::mem::take(&mut b)) {
        Ok(t) => t,
        Err(e) if e.utf8_error().error_len().is_none() => {
            let n = e.utf8_error().valid_up_to();
            let mut v = e.into_bytes();
            v.truncate(n);
            String::from_utf8(v).unwrap_or_default()
        }
        Err(_) => String::new(),
    }
}

/// O_NONBLOCK where its value is known (a FIFO swapped in then opens without waiting for a writer); elsewhere 0.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
const NONBLOCK: i32 = 0x4;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "riscv64"
    )
))]
const NONBLOCK: i32 = 0o4000;
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "riscv64"
        )
    )
)))]
const NONBLOCK: i32 = 0;

/// At most `cap` bytes of the regular file `p`: opened without blocking on a FIFO, and refused unless the opened
/// file is a regular file.
fn read_capped(p: &std::path::Path, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, NONBLOCK);
    let f = o.open(p)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut b = Vec::new();
    f.take(cap).read_to_end(&mut b)?;
    Ok(b)
}

/// Lexer states of [`check_limits`]: outside any string, in a comment, in a single-quoted string (and just after
/// a `'` in one, which is either its end or the first half of `''`), in a double-quoted string (and just after a
/// `\` in one).
const OUT: usize = 0;
const COMMENT: usize = 1;
const SQ: usize = 2;
const SQ_END: usize = 3;
const DQ: usize = 4;
const DQ_ESC: usize = 5;
/// Per lexer state: the largest open flow depth any reading of the text so far reaches in that state.
type States = [Option<usize>; 6];

fn put(s: &mut States, i: usize, f: usize) {
    s[i] = Some(s[i].map_or(f, |g| g.max(f)));
}

/// Where a token may start: a quote here may open a string, and a bracket may open a flow collection.
#[derive(Clone, Copy)]
struct At {
    /// after whitespace or at the line start
    ws_before: bool,
    /// a quote may start a string here (also after whitespace, so a tag or anchor before it is covered)
    quote: bool,
    /// a `[`/`{` may open a flow collection here: after an indicator, at the line start, or after a tag/anchor
    flow: bool,
}

/// One character read outside any string. A quote that may start a string is read both ways, as the start of a
/// string and as plain text, so a wrong guess can only over-count.
fn out_step(next: &mut States, f: usize, c: char, at: At) {
    match c {
        '#' if at.ws_before => put(next, COMMENT, f),
        '[' | '{' if at.flow => put(next, OUT, f + 1),
        ']' | '}' => put(next, OUT, f.saturating_sub(1)),
        '\'' | '"' => {
            put(next, OUT, f);
            if at.quote {
                put(next, if c == '\'' { SQ } else { DQ }, f);
            }
        }
        _ => put(next, OUT, f),
    }
}

/// The lines as a YAML 1.1 reader breaks them: at `\r\n`, `\r`, `\n`, U+0085, U+2028 and U+2029 (an extra empty
/// line from `\r\n` changes nothing). A leading U+FEFF, which the reader skips, does not count as indentation.
pub fn yaml_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\r', '\n', '\u{85}', '\u{2028}', '\u{2029}'])
        .map(|l| l.trim_start_matches('\u{feff}'))
}

/// The size, nesting and alias checks of [`parse`].
///
/// The nesting is an upper bound computed without parsing: per line, the open indentation levels (a `- ` entry at
/// its parent key's own indentation counts as one more), plus the block indicators (`- `, `? `, `: `) starting
/// the line, plus the deepest open flow collection. Flow depth comes from a small lexer that follows every
/// possible reading of each quote character (string start or plain text, with `''` and `\` escapes and strings
/// spanning lines), keeping the largest depth per state; a closing bracket lowers the depth only in the readings
/// where it is outside strings and comments. The text of a `|`/`>` block scalar is skipped only when its indicator
/// line was read outside any string and any flow collection in every reading.
///
/// Aliases are bounded where they are expanded: [`parse_all_nodes`] stops reading past [`max_nodes`].
pub fn check_limits(text: &str) -> Result<(), String> {
    if text.len() > MAX_YAML_BYTES {
        return Err(format!(
            "YAML document too large ({} bytes; the limit is {MAX_YAML_BYTES})",
            text.len()
        ));
    }
    let only = |s: &States, i: usize| s.iter().enumerate().all(|(j, x)| (j == i) == x.is_some());
    let flows = |s: &States| s.iter().flatten().copied().max().unwrap_or(0);
    let too_deep = || {
        Err(format!(
            "YAML document nested too deeply (over {MAX_YAML_DEPTH} levels)"
        ))
    };
    let mut st: States = [Some(0), None, None, None, None, None];
    // open block levels: (indentation, the line was a sequence entry)
    let mut indents: Vec<(usize, bool)> = vec![];
    let mut block: Option<usize> = None;
    for line in yaml_lines(text) {
        let body = line.trim_start_matches([' ', '\t']);
        let ind = line.len() - body.len();
        if block.is_some_and(|b| body.is_empty() || ind > b) {
            continue;
        }
        block = None;
        if body.is_empty() || (only(&st, OUT) && body.starts_with('#')) {
            continue;
        }
        let is_entry = body == "-" || body.starts_with("- ") || body.starts_with("-\t");
        // `k:` then `- x` at the same indentation: the sequence is one level inside the key
        while indents
            .last()
            .is_some_and(|&(top, seq)| top > ind || (top == ind && (seq || !is_entry)))
        {
            indents.pop();
        }
        indents.push((ind, is_entry));
        let mut rest = body;
        let mut indicators = 0;
        while let Some(r) = rest
            .strip_prefix('-')
            .or_else(|| rest.strip_prefix('?'))
            .or_else(|| rest.strip_prefix(':'))
        {
            if !(r.is_empty() || r.starts_with([' ', '\t'])) {
                break;
            }
            indicators += 1;
            rest = r.trim_start_matches([' ', '\t']);
        }
        let rest_col = line.len() - rest.len();
        // `- k:` (a node on the indicator's line): that node's column is a level of its own, so a later `- ` at
        // that column nests one deeper, as after a key on its own line
        if indicators > 0 && !rest.is_empty() && !rest.starts_with('#') {
            indents.push((rest_col, false));
        }
        let mut deepest = flows(&st);
        // the last token read outside any string in every reading: a block scalar indicator?
        let (mut tok, mut tok_done, mut tok_bad, mut tok_frozen) =
            (String::new(), false, false, false);
        let (mut prev, mut prev_sig): (Option<char>, Option<char>) = (None, None);
        // first character of the token before the current one (a tag `!` or an anchor `&` may precede a node)
        let (mut cur_tok_first, mut prev_tok_first): (Option<char>, Option<char>) = (None, None);
        let chars: Vec<char> = rest.chars().collect();
        for &c in chars.iter() {
            let ws = c == ' ' || c == '\t';
            let ws_before = prev.is_none_or(|p| p == ' ' || p == '\t');
            if only(&st, OUT) && !tok_frozen {
                if c == '#' && ws_before {
                    tok_frozen = true;
                } else if ws {
                    tok_done = true;
                } else {
                    if tok_done {
                        tok.clear();
                        tok_done = false;
                    }
                    tok.push(c);
                }
            } else if !only(&st, OUT) && !only(&st, COMMENT) {
                tok_bad = true;
            }
            if !ws && ws_before {
                prev_tok_first = cur_tok_first;
                cur_tok_first = Some(c);
            }
            let after_indicator =
                matches!(prev_sig, None | Some('[' | '{' | ',' | ':' | '?' | '-'));
            let at = At {
                ws_before,
                quote: ws_before || after_indicator,
                flow: after_indicator || (ws_before && matches!(prev_tok_first, Some('!' | '&'))),
            };
            let mut next: States = [None; 6];
            for (i, f) in st.iter().enumerate() {
                let Some(f) = *f else { continue };
                match i {
                    OUT => out_step(&mut next, f, c, at),
                    COMMENT => put(&mut next, COMMENT, f),
                    SQ => put(&mut next, if c == '\'' { SQ_END } else { SQ }, f),
                    SQ_END if c == '\'' => put(&mut next, SQ, f),
                    SQ_END => out_step(&mut next, f, c, at),
                    DQ => put(
                        &mut next,
                        match c {
                            '"' => OUT,
                            '\\' => DQ_ESC,
                            _ => DQ,
                        },
                        f,
                    ),
                    _ => put(&mut next, DQ, f), // DQ_ESC: the escaped character
                }
            }
            st = next;
            deepest = deepest.max(flows(&st));
            prev = Some(c);
            if !ws {
                prev_sig = Some(c);
            }
        }
        let plain_end = !tok_bad && (only(&st, OUT) || only(&st, COMMENT)) && flows(&st) == 0;
        if plain_end
            && tok.starts_with(['|', '>'])
            && tok[1..]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'+' || b == b'-')
        {
            block = Some(rest_col);
        }
        // at the line end a comment ends, a `'` just read closed its string, and a `\` escaped the line break
        let mut next: States = [None; 6];
        for (i, f) in st.iter().enumerate() {
            let Some(f) = *f else { continue };
            let to = match i {
                COMMENT | SQ_END => OUT,
                DQ_ESC => DQ,
                i => i,
            };
            put(&mut next, to, f);
        }
        st = next;
        if indents.len() + indicators + deepest > MAX_YAML_DEPTH {
            return too_deep();
        }
    }
    Ok(())
}

/// YAML text with YAML-1.1 plain booleans spelled true/false (parse the result with [`parse`]).
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
    match parse::<Y>(&out) {
        Ok(back) if back == *v => Ok(out),
        _ => Err("config.yaml re-emit does not round-trip; nothing written".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn y(t: &str) -> Y {
        parse(t).unwrap()
    }

    #[test]
    fn parse_refuses_oversized_and_deeply_nested_documents() {
        let big = format!("a: \"{}\"\n", "x".repeat(MAX_YAML_BYTES));
        let e = parse::<Y>(&big).unwrap_err();
        assert!(e.contains("too large"), "{e}");
        // flow nesting
        let flow = format!("a: {}1{}\n", "[".repeat(200), "]".repeat(200));
        assert!(parse::<Y>(&flow).unwrap_err().contains("nested too deeply"));
        // block nesting by indentation
        let mut block = String::new();
        for i in 0..100 {
            block += &format!("{}k{i}:\n", " ".repeat(i));
        }
        assert!(parse::<Y>(&block)
            .unwrap_err()
            .contains("nested too deeply"));
        // sequences of sequences on one line
        let seq = format!("{}x\n", "- ".repeat(100));
        assert!(parse::<Y>(&seq).unwrap_err().contains("nested too deeply"));
        // ordinary documents, brackets inside quotes and comments, and moderate nesting still parse
        let ok = "a:\n  b: [1, 2, {c: 3}]\n  s: \"[[[[[[\" # {{{{{{\n  t: '{{{{'\nl:\n  - - x\n    - y\n";
        assert!(parse::<Y>(ok).is_ok());
        let deep_ok = format!("a: {}1{}\n", "[".repeat(40), "]".repeat(40));
        assert!(parse::<Y>(&deep_ok).is_ok());
        // a block scalar's text (unbalanced brackets, deep indentation) is not nesting
        let mut text = String::from("prompt: |\n");
        for i in 0..100 {
            text += &format!("  {}see [{i}\n", " ".repeat(i));
        }
        text += "next: {a: 1}\n";
        let v: Y = parse(&text).unwrap();
        assert_eq!(v["next"]["a"], Y::from(1));
        // a quote inside a plain scalar, escaped quotes, `''`, strings spanning lines, comments and a flow line
        // ending in `>` or `|` never hide nesting from the count
        let nest = format!("{}1{}", "[".repeat(100), "]".repeat(100));
        for doc in [
            format!("a: [x'y, {nest}]\n"),
            format!("a: [\"x\\\"\", {nest}]\n"),
            format!("a: ['x''', {nest}]\n"),
            format!("a: [x'y, 'z]]]]]]]]', {nest}]\n"),
            format!("a: [\"x\n  ]]]]]]]]\", {nest}]\n"),
            format!("a: [b, >\n  {nest}]\n"),
            format!("a: [b, |\n  {nest}]\n"),
            format!("a: x # ]]]]]]]]\nb: {nest}\n"),
            format!("- k: |\n  x: {nest}\n"),
        ] {
            let e = parse::<Y>(&doc).map(|_| ()).unwrap_err();
            assert!(e.contains("nested too deeply"), "{doc:?}: {e}");
        }
        // ordinary quoting still reads as balanced
        let mut many = String::new();
        for i in 0..200 {
            many += &format!("k{i}: [\"a]\", 'b''c', \"d\\\"\", \"[e]\", x'y, \"\"]\n");
        }
        assert!(check_limits(&many).is_ok());
        assert!(parse::<Y>(&many).is_ok());
        // line breaks other than \n, explicit keys, a BOM before indicators and indentless sequences are counted
        let mut cr = String::new();
        for i in 0..100 {
            cr += &format!("{}k{i}:\r", " ".repeat(i));
        }
        for doc in [
            format!("a: 1 # c\rb: {nest}\n"),
            format!("a: 1 # c\u{2028}b: {nest}\n"),
            format!("a: 1 # c\u{85}b: {nest}\n"),
            cr,
            format!("{}x\n", "? ".repeat(100)),
            format!("\u{feff}{}x\n", "- ".repeat(100)),
            (0..70)
                .map(|i| format!("{}k{i}:\n{}- \n", "  ".repeat(i), "  ".repeat(i)))
                .collect::<String>(),
        ] {
            let e = parse::<Y>(&doc).map(|_| ()).unwrap_err();
            assert!(e.contains("nested too deeply"), "{doc:?}: {e}");
        }
        // compact entries: `- k:` puts the mapping on the dash's line; the next `- ` at its column nests deeper
        let mut compact = String::from("k0:\n");
        for i in 1..40 {
            compact += &format!("{}- k{i}:\n", "  ".repeat(i - 1));
        }
        assert!(
            check_limits(&compact)
                .unwrap_err()
                .contains("nested too deeply"),
            "39 compact entries nest about 77 levels"
        );
        // indentless sequences: two levels per `k:` + `- ` pair
        let mut seq = String::new();
        for i in 0..40 {
            seq += &format!("{}k{i}:\n{}- \n", "  ".repeat(i), "  ".repeat(i));
        }
        assert!(
            check_limits(&seq).is_err(),
            "40 key + entry pairs are 80 levels"
        );
        // no false positives for an unbalanced bracket in a plain scalar, or a block indicator with a comment
        let mut notes = String::new();
        for i in 0..100 {
            notes += &format!("note{i}: see [{i} and {{x\n");
        }
        notes += "prompt: | # the bot's prompt\n";
        for i in 0..100 {
            notes += &format!("  line [{i} {{\n");
        }
        assert!(parse::<Y>(&notes).is_ok(), "{:?}", check_limits(&notes));
        // a flow collection after a tag or an anchor still counts
        let tagged = format!("a: !!seq {}1{}\n", "[".repeat(100), "]".repeat(100));
        assert!(check_limits(&tagged)
            .unwrap_err()
            .contains("nested too deeply"));
        let anchored = format!("a: &x {}1{}\n", "[".repeat(100), "]".repeat(100));
        assert!(check_limits(&anchored)
            .unwrap_err()
            .contains("nested too deeply"));
    }

    #[test]
    fn yaml_files_are_read_with_the_cap() {
        let d = crate::core::util::test_dir("yread");
        let small = d.join("demo.yaml");
        std::fs::write(&small, "a: 1\n").unwrap();
        assert_eq!(read_yaml_file(&small).unwrap(), "a: 1\n");
        let big = d.join("demo-big.yaml");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_YAML_BYTES as u64 + 10).unwrap(); // sparse: nothing near this size is ever read
        assert!(read_yaml_file(&big).unwrap_err().contains("too large"));
        assert_eq!(read_yaml_prefix(&big).len(), MAX_YAML_BYTES);
        assert_eq!(read_yaml_prefix(&d.join("missing.md")), "");
        // not UTF-8: empty, as before the cap
        let bin = d.join("demo-bin.md");
        std::fs::write(&bin, b"---\nname: \xff\n---\n").unwrap();
        assert_eq!(read_yaml_prefix(&bin), "");
        // a FIFO is refused without waiting for a writer
        let fifo = d.join("demo.fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert!(read_yaml_file(&fifo).is_err());
        assert_eq!(read_yaml_prefix(&fifo), "");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn parse_all_reads_every_document_under_the_limits() {
        let docs = parse_all("a: 1\n---\nb: 2\n...\n---\n- c\n").unwrap();
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[1]["b"], Y::from(2));
        assert!(parse_all("").unwrap().iter().all(|d| d.is_null()));
        let deep = format!("a: 1\n---\nb: {}1{}\n", "[".repeat(100), "]".repeat(100));
        assert!(parse_all(&deep).unwrap_err().contains("nested too deeply"));
        let e = parse_all("demo-key-name: 1\ndemo-key-name: 2\n").unwrap_err();
        assert!(
            e.starts_with("not valid YAML") && !e.contains("demo-key-name"),
            "{e}"
        );
        assert!(parse_all("a: [unclosed\n")
            .unwrap_err()
            .starts_with("not valid YAML"));
    }

    #[test]
    fn parse_bounds_alias_expansion() {
        // one anchor replayed many times: reading stops once the expansion passes the node cap
        let items: String = (0..2000).map(|i| format!("{i},")).collect();
        let mut bomb = format!("a: &a [{items}0]\nb: [");
        for _ in 0..2000 {
            bomb += "*a,";
        }
        bomb += "*a]\n";
        let e = parse::<Y>(&bomb).map(|_| ()).unwrap_err();
        assert!(e.contains("too many nodes"), "{e}");
        // nested replays: few aliases, exponential expansion
        let mut nested = String::from("l0: &l0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..12 {
            let p = i - 1;
            nested += &format!(
                "l{i}: &l{i} [*l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}]\n"
            );
        }
        assert!(parse::<Y>(&nested).unwrap_err().contains("too many nodes"));
        assert!(parse_all_nodes(&nested)
            .unwrap_err()
            .contains("too many nodes"));
        // a long anchored string replayed many times: refused on the text it would expand to, not its node count
        let long = "x".repeat(1024 * 1024);
        let mut replayed = format!("a: &a \"{long}\"\nb: [");
        for _ in 0..100 {
            replayed += "*a, ";
        }
        replayed += "*a]\n";
        let e = parse::<Y>(&replayed).map(|_| ()).unwrap_err();
        assert!(e.contains("too much text"), "{e}");
        // the same string used once is fine
        assert!(parse::<Y>(&format!("a: &a \"{long}\"\nb: [*a]\n")).is_ok());
        // a few aliases in a small document (what PyYAML writes for a shared object) are fine
        let small = "base: &base {model: demo-model, temp: 1}\nprofiles:\n  - *base\n  - *base\n";
        let v: Y = parse(small).unwrap();
        assert_eq!(v["profiles"][1]["model"], Y::from("demo-model"));
        // and so is a real-sized config (about 2 KB) that reuses one block through 15 aliases
        let mut cfg = String::from(
            "defaults: &defaults\n  model: demo-model\n  temperature: 0.2\n  max_tokens: 4096\n  tools: [search, files, shell]\n",
        );
        for i in 0..15 {
            cfg += &format!(
                "profile_{i}:\n  <<: *defaults\n  name: demo profile number {i}\n  description: an example profile for ordinary alias use\n"
            );
        }
        assert!(cfg.len() > 1500, "{}", cfg.len());
        assert!(cfg.matches('*').count() >= 15);
        let v: Y = parse(&cfg).unwrap();
        assert_eq!(v["profile_14"]["name"], Y::from("demo profile number 14"));
    }

    #[test]
    fn no_document_expands_past_the_node_cap() {
        // many small scalars without any alias: over the absolute cap however long the file
        let many = format!("a: [{}0]\n", "0,".repeat(MAX_YAML_NODES + 10));
        assert!(many.len() < MAX_YAML_BYTES);
        assert!(parse::<Y>(&many).unwrap_err().contains("too many nodes"));
        // below it, the same shape reads
        let fewer = format!("a: [{}0]\n", "0,".repeat(MAX_YAML_NODES / 2));
        assert!(parse::<Y>(&fewer).is_ok());
    }

    #[test]
    fn parse_errors_never_quote_the_document() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Front {
            count: u32,
        }
        let e = parse::<Front>("count: demo-scalar-text\n").unwrap_err();
        assert!(e.starts_with("not valid YAML"), "{e}");
        assert!(!e.contains("demo-scalar-text"), "{e}");
        let e = parse::<Y>("a: [demo-unclosed\n").unwrap_err();
        assert!(!e.contains("demo-unclosed"), "{e}");
    }

    #[test]
    fn nodes_keep_every_entry_key_and_tag() {
        let n = parse_all_nodes("a: 1\na: x\n1: one\n\"1\": two\nt: !demo-tag v\n? [k]\n: w\n")
            .unwrap();
        let Node::Map(m) = &n[0] else { panic!("{n:?}") };
        assert_eq!(m.len(), 6, "duplicate keys are kept: {m:?}");
        assert_eq!(m[0], (Node::Str("a".into()), Node::Number("1".into())));
        assert_eq!(m[1], (Node::Str("a".into()), Node::Str("x".into())));
        assert!(
            matches!(&m[4].1, Node::Tagged(t, v) if t.contains("demo-tag") && **v == Node::Str("v".into())),
            "{m:?}"
        );
        assert!(matches!(&m[5].0, Node::Seq(_)), "{m:?}");
        assert_eq!(parse_all_nodes("a: 1\n---\nb: 2\n").unwrap().len(), 2);
    }

    /// Every YAML parse in the sources goes through `parse` (and so through the limits).
    #[test]
    fn all_yaml_parsing_goes_through_parse() {
        fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![];
        walk(&root.join("src"), &mut files);
        walk(&root.join("crates/g2h/src"), &mut files);
        let calls: Vec<String> = ["from_str", "from_slice", "from_reader", "Deserializer"]
            .iter()
            .map(|c| format!("serde_yaml::{c}"))
            .collect();
        let mut bad = vec![];
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            for (i, line) in text.lines().enumerate() {
                if calls.iter().any(|c| line.contains(c.as_str())) {
                    bad.push(format!("{}:{}", f.display(), i + 1));
                }
            }
        }
        // the calls inside `parse`, `parse_all` and `parse_all_nodes`, each after the limits and the node cap
        assert!(!bad.is_empty() && bad.len() <= 3, "{bad:?}");
        assert!(bad.iter().all(|b| b.contains("yaml11.rs")), "{bad:?}");
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

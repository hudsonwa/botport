//! The secret detector, two rules. Text is never rewritten.
//! (A) Routing ([`scan`], [`connector_url`], [`credential_name`]): keyword-based and conservative, for schedule
//! prompts/fields, connectors and credential names only. Any secret keyword routes the item to the manual checklist
//! ("token budget" too, on purpose; `key` alone does not); never an error.
//! (B) The write guard ([`guard`], [`guard_str`], [`guard_args`]): high precision, no keyword rule, on every written
//! string; a hit is a hard refusal naming the field, never the value.

/// Why a text looks like it holds a secret (no part of the text itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reason(pub &'static str);

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

use regex::Regex;
use std::sync::OnceLock;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

/// Words that name a secret, matched against each word of a key (`client_secret`, `x-api-key`, `sessionToken`).
const KEY_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "pw",
    "pass",
    "passphrase",
    "passcode",
    "secret",
    "token",
    "key",
    "apikey",
    "auth",
    "authorization",
    "bearer",
    "credential",
    "credentials",
    "pin",
    "cookie",
    "session",
    "sessionid",
    "jsessionid",
    "sid",
    "pat",
    "otp",
    "sig",
    "signature",
    "privkey",
];
/// Also secret-naming when run together with other words (`PGPASSWORD`, `accesstoken`).
const KEY_PARTS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "passcode",
    "secret",
    "apikey",
    "credential",
    "token",
];
/// After `is` or a bare word these are prose, not a value.
const STOP: &[&str] = &[
    "in",
    "the",
    "a",
    "an",
    "to",
    "from",
    "for",
    "of",
    "and",
    "or",
    "on",
    "at",
    "with",
    "is",
    "are",
    "not",
    "none",
    "stored",
    "kept",
    "here",
    "there",
    "it",
    "this",
    "that",
    "them",
    "by",
    "as",
    "set",
    "required",
    "needed",
    "missing",
    "valid",
    "invalid",
    "expired",
    "available",
    "located",
    "same",
    "unchanged",
    "empty",
    "manager",
    "field",
    "reset",
    "policy",
    "change",
    "protected",
    "prompt",
    "lives",
    "goes",
    "must",
    "should",
    "will",
    "can",
    "has",
    "was",
    "be",
    "via",
    "using",
    "when",
    "if",
    "you",
    "your",
];

/// Split an identifier into lowercase words: `_` `-` `.` and camelCase boundaries; trailing digits dropped.
fn key_words(k: &str) -> Vec<String> {
    let mut out = vec![];
    for part in k.split(['_', '-', '.']) {
        let c: Vec<char> = part.chars().collect();
        let mut w = String::new();
        for (i, &ch) in c.iter().enumerate() {
            let boundary = i > 0
                && ch.is_uppercase()
                && (c[i - 1].is_lowercase()
                    || c[i - 1].is_ascii_digit()
                    || c.get(i + 1).is_some_and(|n| n.is_lowercase()) && c[i - 1].is_uppercase());
            if boundary && !w.is_empty() {
                out.push(std::mem::take(&mut w));
            }
            w.extend(ch.to_lowercase());
        }
        if !w.is_empty() {
            out.push(w);
        }
    }
    out.into_iter()
        .map(|w| w.trim_end_matches(|c: char| c.is_ascii_digit()).to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

fn secret_key(k: &str) -> bool {
    let words = key_words(k);
    if words.iter().any(|w| KEY_WORDS.contains(&w.as_str())) {
        return true;
    }
    let joined = words.concat();
    KEY_PARTS.iter().any(|p| {
        joined
            .match_indices(p)
            .any(|(i, _)| !(*p == "token" && joined[i + p.len()..].starts_with('s')))
    })
}

/// High-entropy word: 10+ chars mixing upper, lower and digits, or 16+ mixing letters and digits.
fn high_entropy(w: &str) -> bool {
    let (u, l, d) = (
        w.bytes().any(|b| b.is_ascii_uppercase()),
        w.bytes().any(|b| b.is_ascii_lowercase()),
        w.bytes().any(|b| b.is_ascii_digit()),
    );
    (w.len() >= 10 && u && l && d) || (w.len() >= 16 && (u || l) && d)
}

/// A word that reads like a value, not prose: quoted, has a digit, an inner capital, `=`-padding, or is very long.
fn value_like(v: &str) -> bool {
    v.starts_with(['"', '\'', '`'])
        || v.bytes().any(|b| b.is_ascii_digit())
        || v.chars().skip(1).any(|c| c.is_uppercase())
        || v.ends_with('=')
        || v.chars().count() >= 20
}

macro_rules! kw {
    ($($k:literal),*) => {
        &[$(($k, concat!("mentions a secret keyword ('", $k, "'); review and add by hand if it holds no secret"))),*]
    };
}
/// Matched inside any word part (so `tokenize`, `secretary`, `max_tokens` collide, on purpose).
const KW_PART: &[(&str, &str)] = kw![
    "password",
    "passwd",
    "passphrase",
    "passcode",
    "secret",
    "token",
    "apikey",
    "bearer",
    "credential"
];
/// Matched only as a whole word part (`userPwd`, `otp_code`), not inside `output` / `hotpot` / `laptop`.
const KW_WHOLE: &[(&str, &str)] = kw!["pwd", "otp", "2fa", "pw", "creds", "pin"];
/// Two consecutive word parts, or both run together in one (`api key`, `API-Key`, `sshKey`, `PinCode`).
const KW_PHRASE: &[(&str, &str, &str)] = &[
    ("api", "key", kw!["api key"][0].1),
    ("private", "key", kw!["private key"][0].1),
    ("ssh", "key", kw!["ssh key"][0].1),
    ("pin", "code", kw!["pin code"][0].1),
];

/// Lowercase word parts of a text: split on non-alphanumerics and camelCase (`sendOTPCode` -> send, otp, code).
fn parts(t: &str) -> Vec<String> {
    let mut out = vec![];
    for w in t.split(|c: char| !c.is_alphanumeric()) {
        let c: Vec<char> = w.chars().collect();
        let mut p = String::new();
        for (i, &ch) in c.iter().enumerate() {
            let boundary = i > 0
                && ch.is_uppercase()
                && (c[i - 1].is_lowercase()
                    || c[i - 1].is_uppercase() && c.get(i + 1).is_some_and(|n| n.is_lowercase()));
            if boundary {
                out.push(std::mem::take(&mut p));
            }
            p.extend(ch.to_lowercase());
        }
        out.push(p);
    }
    out.retain(|p| !p.is_empty());
    out
}

/// Leet folded in each word that has a letter (`p@$$w0rd` -> `password`); `1` reads as `i` or as `l`.
fn deleet(t: &str, one: char) -> String {
    t.split_inclusive(char::is_whitespace)
        .map(|w| {
            if !w.chars().any(|c| c.is_ascii_alphabetic()) {
                return w.to_string();
            }
            w.chars()
                .map(|c| match c {
                    '@' | '4' => 'a',
                    '$' | '5' => 's',
                    '0' => 'o',
                    '3' => 'e',
                    '7' => 't',
                    '1' => one,
                    _ => c,
                })
                .collect()
        })
        .collect()
}

/// Runs of spaced single letters joined (`p a s s w o r d` -> `password`).
fn unspace(t: &str) -> String {
    static S: OnceLock<Regex> = OnceLock::new();
    re(&S, r"\b[A-Za-z](?:[ \t]+[A-Za-z]\b)+")
        .replace_all(t, |c: &regex::Captures| {
            c[0].split_whitespace().collect::<String>()
        })
        .into_owned()
}

fn keyword(t: &str) -> Option<Reason> {
    let t = unspace(t);
    [
        t.clone(),
        unspace(&deleet(&t, 'i')),
        unspace(&deleet(&t, 'l')),
    ]
    .iter()
    .find_map(|t| keyword_plain(t))
}

fn keyword_plain(t: &str) -> Option<Reason> {
    let ps = parts(t);
    // Then adjacent parts run together (`Pass-Phrase`, `pass word`, `to.ken`): any separator, any case.
    let mut joined = vec![];
    for w in [2, 3] {
        joined.extend(ps.windows(w).map(|x| x.concat()));
    }
    keyword_in(&ps).or_else(|| keyword_in(&joined))
}

fn keyword_in(ps: &[String]) -> Option<Reason> {
    let part = ps
        .iter()
        .find_map(|p| KW_PART.iter().find(|(k, _)| p.contains(k)));
    let whole = ps
        .iter()
        .find_map(|p| KW_WHOLE.iter().find(|(k, _)| p == k));
    let phrase = KW_PHRASE.iter().find(|(a, b, _)| {
        let ab = format!("{a}{b}");
        ps.iter().any(|p| p.contains(&ab)) || ps.windows(2).any(|w| w[0] == *a && w[1] == *b)
    });
    part.map(|(_, r)| Reason(r))
        .or(phrase.map(|(_, _, r)| Reason(r)))
        .or(whole.map(|(_, r)| Reason(r)))
}

/// Unicode format characters (Cf: zero-width, soft hyphen, bidi controls, tags) that hide inside a word.
fn format_char(c: char) -> bool {
    matches!(c as u32,
        0xAD | 0x61C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB | 0xE0000..=0xE007F)
}

/// Format characters (zero-width etc.) dropped, then NFKC (full-width ASCII, ligatures, compatibility forms to plain).
fn fold(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    text.chars()
        .filter(|&c| !format_char(c))
        .nfkc()
        .map(|c| if c == '\u{3000}' { ' ' } else { c })
        .collect()
}

/// A word mixing ASCII letters with non-Latin letters (`раssword` with Cyrillic р/а): a homoglyph can hide a keyword.
fn mixed_script(t: &str) -> Option<Reason> {
    let latin = |c: char| c.is_ascii() || matches!(c as u32, 0xC0..=0x24F | 0x1E00..=0x1EFF);
    t.split(|c: char| !c.is_alphanumeric())
        .any(|w| {
            w.chars().any(|c| c.is_ascii_alphabetic())
                && w.chars().any(|c| c.is_alphabetic() && !latin(c))
        })
        .then_some(Reason(
            "mixed-script word (ASCII letters next to look-alike letters); review and add by hand",
        ))
}

/// Lowercase words joined by `-` (`aws-sdk-v3-usage`), each part short: a slug or name.
fn slug(v: &str) -> bool {
    let ps: Vec<&str> = v.split('-').collect();
    ps.len() >= 2
        && ps.iter().all(|p| {
            (1..=12).contains(&p.len())
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn stop(v: &str) -> bool {
    STOP.iter()
        .any(|s| s.eq_ignore_ascii_case(v.trim_matches(['"', '\'', '`'])))
}

/// `key=value`, `key: value`, `key is value`, `key header value`, `key value` with a secret-naming key.
fn assignment(t: &str) -> Option<Reason> {
    static K: OnceLock<Regex> = OnceLock::new();
    static A: OnceLock<Regex> = OnceLock::new();
    static N: OnceLock<Regex> = OnceLock::new();
    let after = re(
        &A,
        r#"^["'`*]*\s*(?:(?P<op>[:=：])\s*|\s+(?P<is>is|are|was|header)(?:\s+(?:a|an|the))?\s+|(?P<sp>\s+))(?P<v>"[^"]*"|'[^']*'|`[^`]*`|[^\s,;)\]]+)"#,
    );
    for k in re(&K, r"[A-Za-z0-9][A-Za-z0-9_.\-]*").find_iter(t) {
        if !secret_key(k.as_str().trim_end_matches(['.', '-'])) {
            continue;
        }
        if let Some(c) = after.captures(&t[k.end()..]) {
            let v = c["v"].trim_end_matches('.');
            // A bare `[` / `{` / `{}` opens a JSON array or object, it is not a value.
            if !stop(v) && !matches!(v.trim_matches(['"', '\'', '`']), "" | "[" | "{" | "{}") {
                if c.name("op").is_some() {
                    return Some(Reason(
                        "a credential-named key with a value (key=value / key: value)",
                    ));
                }
                if c.name("is").is_some() {
                    return Some(Reason("plain-language secret (\"the password is ...\")"));
                }
                // a kebab-case slug (`aws-secrets-manager aws-sdk-v3-usage`, a list of skill names) is a name, not a value
                if value_like(v) && !slug(v) {
                    return Some(Reason("a credential word followed by a value-like word"));
                }
            }
        }
        // `API_TOKEN (in ~/.env: v4lue)`: a value assigned shortly after the key.
        let near = &t[k.end()..];
        let near = &near[..near.char_indices().nth(48).map_or(near.len(), |(i, _)| i)];
        if re(&N, r"[:=：]\s*([^\s,;)\]]+)")
            .captures_iter(near)
            .any(|c| {
                let v = &c[1];
                v.len() >= 6
                    && v.bytes().any(|b| b.is_ascii_digit())
                    && v.bytes().any(|b| b.is_ascii_alphabetic())
            })
        {
            return Some(Reason("a credential-named key near a value"));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
        {
            let hex = |c: u8| (c as char).to_digit(16).unwrap() as u8;
            out.push(hex(b[i + 1]) << 4 | hex(b[i + 2]));
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Userinfo, a credential-ish query/fragment/matrix part, or a credential in the path (percent-encoding decoded first).
fn url(u: &str) -> Option<Reason> {
    static R: OnceLock<Regex> = OnceLock::new();
    let raw_authority = u
        .split_once("://")?
        .1
        .split(['/', '?', '#', ';'])
        .next()
        .unwrap_or("");
    if raw_authority.contains('@') {
        return Some(Reason("url with userinfo (user:password@host)"));
    }
    // A percent-encoded `?`, `#`, `;`, `&` or `=` smuggles a query or fragment into the path.
    let lower = u.to_ascii_lowercase();
    if ["%3f", "%23", "%3b", "%26", "%3d"]
        .iter()
        .any(|e| lower.contains(e))
    {
        return Some(Reason("percent-encoded query or fragment in a url"));
    }
    let d = percent_decode(u);
    let c = re(&R, r"^[A-Za-z][A-Za-z0-9+.\-]*://([^/?#;]*)([^?#;]*)(.*)$").captures(&d)?;
    if c[1].contains('@') {
        return Some(Reason("url with userinfo (user:password@host)"));
    }
    for part in c[3].split(['?', '&', '#', ';']).filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or(("", part));
        let fragment_kv = !k.is_empty() && c[3][..c[3].find(part).unwrap_or(0)].contains('#');
        if fragment_kv
            || (!k.is_empty() && (secret_key(k) || key_words(k).iter().any(|w| w == "code")))
            || high_entropy(v)
            || (k.is_empty() && value_like(v))
            || v.chars().count() >= 24
        {
            return Some(Reason("credential-ish url query, fragment or ;params"));
        }
    }
    let segs: Vec<&str> = c[2].split('/').filter(|x| !x.is_empty()).collect();
    for (i, s) in segs.iter().enumerate() {
        if i + 1 < segs.len() && KEY_WORDS.contains(&s.to_ascii_lowercase().as_str()) {
            return Some(Reason("credential in a url path"));
        }
    }
    None
}

/// A high-entropy value in a value position (`k=v`, `k: v`, `k is v`). Routing (A) only: the write guard (B) never
/// judges entropy, so a name, slug, id, path or hash can't be refused (supervisor 16:46).
fn value_entropy(t: &str) -> Option<Reason> {
    static W: OnceLock<Regex> = OnceLock::new();
    // entropy only in a value position (`k=v`, `k: v`, `k is v`), never on a bare word, path, file name or hex id
    if re(
        &W,
        r#"(?:[=:：]|\b(?:is|are|was)\b)\s*["'`]?([^\s"'`,;)\]]+)"#,
    )
    .captures_iter(t)
    .any(|c| {
        let v = c[1].trim_end_matches('.');
        !v.contains(['/', '\\', '.', '~'])
            && !v.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            && !slug_like(v)
            && high_entropy(v)
    }) {
        return Some(Reason(HIGH_ENTROPY));
    }
    None
}

fn token(t: &str) -> Option<Reason> {
    static P: OnceLock<Regex> = OnceLock::new();
    static AK: OnceLock<Regex> = OnceLock::new();
    static B: OnceLock<Regex> = OnceLock::new();
    static J: OnceLock<Regex> = OnceLock::new();
    if t.contains("-----BEGIN") && t.contains("PRIVATE KEY") {
        return Some(Reason("private key block"));
    }
    if re(
        &P,
        r"(?:^|[^A-Za-z0-9])(?:sk-|xai-|gh[pousr]_|github_pat_|xox[abposr]-|glpat-)[A-Za-z0-9_\-]{8,}",
    )
    .is_match(t)
        || re(&AK, r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").is_match(t)
    {
        return Some(Reason("token-shaped value (sk-/xai-/gh*_/xox*-/AKIA...)"));
    }
    if re(&J, r"\beyJ[A-Za-z0-9_\-]{5,}\.[A-Za-z0-9_\-]{5,}").is_match(t) {
        return Some(Reason("JWT"));
    }
    if let Some(c) = re(&B, r"(?i)\b(?:bearer|basic)\s+([A-Za-z0-9._~+/\-]+=*)").captures(t) {
        if value_like(&c[1]) || c[1].len() >= 16 {
            return Some(Reason("bearer/basic auth value"));
        }
    }
    None
}

/// The per-field scan switched off ([`scan`], [`scan_value`], [`credential_name`] say clean) so a test can show the
/// write-boundary guard alone keeps a secret off disk. Honoured only in debug builds.
fn field_scan_off() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("G2H_TEST_SKIP_FIELD_SCAN").is_some_and(|v| !v.is_empty())
}

/// Rule (A): `Some(reason)` when `text` mentions a secret keyword or looks like it holds a credential value.
pub fn scan(text: &str) -> Option<Reason> {
    if field_scan_off() {
        return None;
    }
    scan_all(text)
}

fn scan_all(text: &str) -> Option<Reason> {
    let f = fold(text);
    mixed_script(&f)
        .or_else(|| keyword(&f))
        .or_else(|| value_scan(text))
}

/// [`scan`] without the keyword rule, for text that names a secret by design (a skill file in a bundle): a
/// value-like credential, or a password word followed by any word that is not prose (`my password hunter`).
pub fn scan_value(text: &str) -> Option<Reason> {
    if field_scan_off() {
        return None;
    }
    value_scan(text)
}

/// A connector url: [`scan_value`], and any fragment or any query value that is not a plain flag or number.
pub fn connector_url(u: &str) -> Option<Reason> {
    const OK: &[&str] = &["true", "false", "yes", "no", "on", "off", "json", "sse"];
    if let Some(r) = scan_value(u) {
        return Some(r);
    }
    if field_scan_off() {
        return None;
    }
    if u.contains('#') {
        return Some(Reason("connector url with a fragment"));
    }
    let q = u.split_once('?').map_or("", |(_, q)| q);
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').map_or("", |(_, v)| v))
        .any(|v| !(OK.contains(&v) || v.len() <= 4 && v.bytes().all(|b| b.is_ascii_digit())))
        .then_some(Reason("connector url with a query value"))
}

fn value_scan(text: &str) -> Option<Reason> {
    let text = fold(text);
    let text = text.as_str();
    static U: OnceLock<Regex> = OnceLock::new();
    static PW: OnceLock<Regex> = OnceLock::new();
    if let Some(r) = mixed_script(text) {
        return Some(r);
    }
    if re(
        &PW,
        r#"(?i)\b(?:pass[\s_.\-]*(?:word|phrase|code)|passwd)s?\b[\s:=："'`]+([^\s,;)\]]+)"#,
    )
    .captures_iter(text)
    .any(|c| !stop(&c[1]))
    {
        return Some(Reason(
            "a password word followed by a word (bare `password value`)",
        ));
    }
    if let Some(r) = re(&U, r#"[A-Za-z][A-Za-z0-9+.\-]*://[^\s)>"'`]+"#)
        .find_iter(text)
        .find_map(|m| url(m.as_str().trim_end_matches(['.', ','])))
    {
        return Some(r);
    }
    // spaced letters of a credential word (`p a s s zq..`) joined, so the word reads as one
    token(text)
        .or_else(|| value_entropy(text))
        .or_else(|| assignment(text))
        .or_else(|| {
            let u = unspace(text);
            (u != text).then(|| assignment(&u)).flatten()
        })
}

/// Words a credential name is commonly built from; any other all-lowercase part of 6+ letters may be a value.
const NAME_WORDS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "passcode",
    "secret",
    "secrets",
    "token",
    "tokens",
    "apikey",
    "bearer",
    "credential",
    "credentials",
    "cookie",
    "session",
    "signature",
    "privkey",
    "access",
    "refresh",
    "client",
    "account",
    "private",
    "public",
    "server",
    "service",
    "webhook",
    "database",
    "bucket",
    "region",
    "tenant",
    "project",
    "profile",
    "endpoint",
    "openai",
    "github",
    "gitlab",
    "google",
    "telegram",
    "discord",
    "twitter",
    "stripe",
    "notion",
    "twilio",
    "sendgrid",
    "mailgun",
    "postgres",
    "mysql",
    "mongodb",
    "redis",
    "anthropic",
    "azure",
    "amazon",
    "dropbox",
    "airtable",
    "hubspot",
    "shopify",
    "openrouter",
    "mistral",
    "cohere",
    "deepseek",
    "huggingface",
    "replicate",
    "pinecone",
    "supabase",
    "firebase",
    "cloudflare",
    "vercel",
    "heroku",
    "netlify",
    "linear",
    "calendar",
    "gmail",
    "outlook",
    "office",
    "microsoft",
    "personal",
    "secondary",
    "primary",
    "backup",
    "admin",
    "master",
    "signing",
    "encryption",
    "username",
    "user",
    "login",
    "email",
    "address",
    "smtp",
    "imap",
    "oauth",
    "consumer",
    "application",
    "app",
    "bot",
    "chat",
    "channel",
    "workspace",
    "organization",
    "default",
];

/// `Some(reason)` unless `name` is a plain identifier (`[A-Za-z][A-Za-z0-9_.-]{0,63}`) with no value in it: no
/// value-like credential, and no unknown all-lowercase part of 6+ letters (`PW_zqhunterpass`); errs suspicious.
pub fn credential_name(name: &str) -> Option<Reason> {
    if field_scan_off() {
        return None;
    }
    cred_name(name)
}

fn cred_name(name: &str) -> Option<Reason> {
    static ID: OnceLock<Regex> = OnceLock::new();
    if !re(&ID, r"^[A-Za-z][A-Za-z0-9_.\-]{0,63}$").is_match(name) {
        return Some(Reason("credential name is not a plain identifier"));
    }
    if let Some(r) = value_scan(name) {
        return Some(r);
    }
    let mut segs = vec![];
    for part in name.split(['_', '-', '.']) {
        let mut seg = String::new();
        for c in part.chars() {
            if c.is_ascii_uppercase() && seg.ends_with(|p: char| p.is_ascii_lowercase()) {
                segs.push(std::mem::take(&mut seg));
            }
            seg.push(c);
        }
        segs.push(seg);
    }
    segs.iter()
        .map(|s| s.trim_end_matches(|c: char| c.is_ascii_digit()))
        .any(|s| {
            s.len() >= 6 && s.bytes().all(|b| b.is_ascii_lowercase()) && !NAME_WORDS.contains(&s)
        })
        .then_some(Reason("credential name may contain a value"))
}

/// Rule (B), the write guard: every written string (bundle files, describe.json, target config, skills, memories, the
/// checklist, scheduler argv) goes through it. Every string of a JSON or YAML document (keys too; YAML by a `.yaml`/`.yml`
/// name), or every line of any other text, gets [`guard_str`]. High precision, no keyword rule: "rotate the API token
/// weekly" passes, `api_key=abc123def456` fails. The first hit fails the write with the field path (or line number)
/// and the reason, never the value. Binary data is not text and is not checked.
pub fn guard(what: &str, data: &[u8]) -> Result<(), String> {
    guard_with(what, data, guard_str)
}

fn guard_with(what: &str, data: &[u8], check: fn(&str) -> Option<Reason>) -> Result<(), String> {
    let Ok(text) = std::str::from_utf8(data) else {
        return Ok(());
    };
    let hit = |at: String, r: Reason| {
        Err(format!(
            "refusing to write {what}: {at} looks like it holds a secret ({r}); route it to the checklist"
        ))
    };
    let jsonl = || {
        let v: Option<Vec<serde_json::Value>> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).ok())
            .collect();
        v.filter(|v| !v.is_empty()).map(serde_json::Value::Array)
    };
    let yaml = || {
        [".yaml", ".yml"]
            .iter()
            .any(|e| what.ends_with(e))
            .then(|| serde_yaml::from_str::<serde_json::Value>(text).ok())
            .flatten()
    };
    let tree = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .or_else(jsonl)
        .or_else(yaml);
    match tree {
        Some(v) => {
            let mut stack = vec![("$".to_string(), &v)];
            while let Some((at, v)) = stack.pop() {
                match v {
                    serde_json::Value::String(t) => {
                        if let Some(r) = check(t) {
                            return hit(at, r);
                        }
                    }
                    serde_json::Value::Array(a) => {
                        stack.extend(a.iter().enumerate().map(|(i, x)| (format!("{at}[{i}]"), x)))
                    }
                    serde_json::Value::Object(m) => {
                        for (k, x) in m {
                            if let Some(r) = check(k) {
                                return hit(format!("a key under {at}"), r);
                            }
                            stack.push((format!("{at}.{k}"), x));
                        }
                    }
                    _ => {}
                }
            }
        }
        None => {
            for (n, l) in text.lines().enumerate() {
                if let Some(r) = check(l) {
                    return hit(format!("line {}", n + 1), r);
                }
            }
        }
    }
    Ok(())
}

const HIGH_ENTROPY: &str = "long high-entropy string";

/// The tool's own texts that read like a value: [`Reason`]s the checklist quotes, and summary labels.
const VALUE_LIKE_REASONS: &[&str] = &[
    "left out as secrets:",
    "looks like it holds a secret",
    "credential (name not shown)",
    "credential-like",
    "credential line",
    "[line with a secret value removed]",
    "Files left out as secrets",
    "**Credentials** — never exported",
    "without any secret values",
    "which credentials exist (names only)",
    "## Credentials",
    "mixed-script word (ASCII letters next to look-alike letters); review and add by hand",
    "a credential word followed by a value-like word",
    "a credential-named key near a value",
    "bearer/basic auth value",
    "connector url with a fragment",
    "connector url with a query value",
    "credential in a url path",
    "credential name is not a plain identifier",
    "credential name may contain a value",
    "credential-ish url query, fragment or ;params",
    "percent-encoded query or fragment in a url",
    "private key block",
    "token-shaped value (sk-/xai-/gh*_/xox*-/AKIA...)",
    "a credential-named key with a value (key=value / key: value)",
    "a password word followed by a word (bare `password value`)",
    "url with userinfo (user:password@host)",
    "plain-language secret (\"the password is ...\")",
];

/// Blank the digests and ids the tool writes itself (`sha256:` + hex, 12/40/64 hex, a UUID, a UTC stamp), the reason texts of [`VALUE_LIKE_REASONS`] and the
/// checklist's `credential NAME` for a name [`credential_name`] passes.
fn blank(t: &str) -> String {
    let mut t = t.to_string();
    for r in VALUE_LIKE_REASONS {
        t = t.replace(r, "#");
    }
    static K: OnceLock<Regex> = OnceLock::new();
    let t = re(
        &K,
        r"mentions a secret keyword \('[a-z0-9 ]+'\); review and add by hand if it holds no secret",
    )
    .replace_all(&t, "#");
    let t = t.into_owned();
    static C: OnceLock<Regex> = OnceLock::new();
    let t = re(
        &C,
        r"\bcredential ([A-Za-z][A-Za-z0-9_.\-]{0,63})\b|\(name ([A-Za-z][A-Za-z0-9_.\-]{0,63})\)",
    )
    .replace_all(&t, |c: &regex::Captures| {
        match cred_name(c.get(1).or(c.get(2)).map_or("", |m| m.as_str())) {
            None => "#".to_string(),
            Some(_) => c[0].to_string(),
        }
    });
    static D: OnceLock<Regex> = OnceLock::new();
    let d = re(
        &D,
        r"\b(?:(?:sha256:)?(?:[0-9a-f]{64}|[0-9a-f]{40}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})|\d{8}T\d{6}Z)\b",
    );
    let t = d.replace_all(&t, "#");
    t.into_owned()
}

/// Rule (B) on one string (after [`blank`]): a token shape (private key, JWT, bearer, high entropy in a value
/// position), a url with userinfo or a credential query/fragment/matrix part, or an explicit `key=value` /
/// `key: value` whose key names a secret and whose value is real (not empty, a prose word or a pure template).
pub fn guard_str(t: &str) -> Option<Reason> {
    static U: OnceLock<Regex> = OnceLock::new();
    static A: OnceLock<Regex> = OnceLock::new();
    static T: OnceLock<Regex> = OnceLock::new();
    let t = fold(&blank(t));
    if let Some(r) = token(&t) {
        return Some(r);
    }
    if let Some(r) = re(&U, r#"[A-Za-z][A-Za-z0-9+.\-]*://[^\s)>"'`]+"#)
        .find_iter(&t)
        .find_map(|m| url_creds(m.as_str().trim_end_matches(['.', ','])))
    {
        return Some(r);
    }
    let template = re(
        &T,
        r"^(?:\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*|\{\{[^}]*\}\}|<[^>]*>|\*+|x+|\.\.\.)$",
    );
    // each key on its own (a quoted value of one key must not swallow the next `key=value`)
    static K: OnceLock<Regex> = OnceLock::new();
    let after = re(
        &A,
        r#"^["'`]?\s*[:=：]\s*("[^"]*"|'[^']*'|`[^`]*`|[^\s"'`,;)\]]+)"#,
    );
    re(&K, r"[A-Za-z0-9][A-Za-z0-9_.\-]*")
        .find_iter(&t)
        .filter(|k| secret_key(k.as_str()))
        .filter_map(|k| after.captures(&t[k.end()..]).map(|c| (k.as_str(), c)))
        .any(|(k, c)| {
            let v = c[1].trim_matches(['"', '\'', '`']).trim_end_matches('.');
            // `pass`/`pw` alone is ambiguous (`PASS: 17/17`, `dossier-pass: review`): it needs a value-like value
            // with a letter in it
            let w = key_words(k);
            let ambiguous = w
                .iter()
                .all(|x| !KEY_WORDS.contains(&x.as_str()) || x == "pass" || x == "pw")
                && !KEY_PARTS.iter().any(|p| w.concat().contains(p));
            // `session_id`, `client_id`, `token_name`: the key names an identifier, not a secret
            let id = matches!(w.last().map(String::as_str), Some("id" | "ids" | "name"));
            !id && !(ambiguous && (!value_like(v) || !v.chars().any(char::is_alphabetic)))
                && v.chars().count() >= 3
                && !stop(v)
                && !template.is_match(v)
                && !phrase(v)
                && !matches!(v, "[" | "{" | "{}" | "[]" | "null" | "true" | "false")
        })
        .then_some(Reason(
            "a credential-named key with a value (key=value / key: value)",
        ))
}

/// Three or more lowercase words joined by `_` or `-` (`run_the_suite`): a phrase or slug, not a value.
fn phrase(v: &str) -> bool {
    static P: OnceLock<Regex> = OnceLock::new();
    re(&P, r"^[a-z]+(?:[_\-][a-z]+){2,}$").is_match(v)
}

/// A lowercase slug (`some-sdk-v3-usage`, `image-to-png-pack`): 3+ parts joined by `-`/`_`, each all letters or a short
/// alphanumeric like `v3`/`x2`/`mp4`. A skill name or id, not a value.
fn slug_like(v: &str) -> bool {
    static S: OnceLock<Regex> = OnceLock::new();
    re(
        &S,
        r"^(?:[a-z]+|[a-z0-9]{1,3})(?:[_\-](?:[a-z]+|[a-z0-9]{1,3})){2,}$",
    )
    .is_match(v)
}

/// A url's credentials only: userinfo, or a query/fragment/matrix part with a secret-named key or a high-entropy value.
fn url_creds(u: &str) -> Option<Reason> {
    let (_, rest) = u.split_once("://")?;
    let d = percent_decode(rest);
    let end = d.find(['/', '?', '#', ';']).unwrap_or(d.len());
    if d[..end].contains('@') {
        return Some(Reason("url with userinfo (user:password@host)"));
    }
    let tail = d.find(['?', '#', ';']).map_or("", |i| &d[i..]);
    tail.split(['?', '&', '#', ';'])
        .filter(|p| !p.is_empty())
        .any(|p| {
            let (k, v) = p.split_once('=').unwrap_or(("", p));
            !k.is_empty() && secret_key(k) && !v.is_empty()
        })
        .then_some(Reason("credential-ish url query, fragment or ;params"))
}

/// [`guard`] for the arguments of a subprocess (which writes its own files): each argument, by label, gets
/// [`guard_str`]; the first hit fails with the label and the reason, never the value.
pub fn guard_args(what: &str, args: &[(&str, &str)]) -> Result<(), String> {
    match args.iter().find_map(|(l, a)| guard_str(a).map(|r| (l, r))) {
        Some((l, r)) => Err(format!(
            "refusing to run {what}: argument {l} looks like it holds a secret ({r}); route it to the checklist"
        )),
        None => Ok(()),
    }
}

/// A crude display string for the checklist only (never written anywhere else): the first few plain words.
pub fn display(text: &str) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .take_while(|w| {
            w.chars().all(|c| c.is_alphabetic() || "'(),!?".contains(c))
                && !secret_key(w)
                && value_scan(w).is_none()
        })
        .take(4)
        .collect();
    match (
        words.is_empty(),
        words.len() < text.split_whitespace().count(),
    ) {
        (true, _) => "(text not shown)".into(),
        (false, true) => format!("{} …", words.join(" ")),
        (false, false) => words.join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEEDED: &str = include_str!("../../tests/fixtures/describe/seeded.md");
    const SEEDED_SECRETS: &str = include_str!("../../tests/fixtures/describe/seeded-secrets.txt");

    fn flagged(t: &str) {
        assert!(scan(t).is_some(), "not flagged: {t:?}");
    }

    #[test]
    fn the_guard_passes_every_reason_and_display_but_no_seeded_secret() {
        let secrets: Vec<&str> = SEEDED_SECRETS.lines().filter(|l| !l.is_empty()).collect();
        for l in SEEDED.lines().chain(secrets.iter().copied()) {
            if let Some(r) = scan(l) {
                let shown = format!("- {} — looks like it holds a secret ({r})", display(l));
                assert_eq!(guard("t", shown.as_bytes()), Ok(()), "{l}");
                let j =
                    serde_json::json!({"reason": r.to_string(), "name": display(l)}).to_string();
                assert_eq!(guard("t", j.as_bytes()), Ok(()), "{l}");
            }
        }
        // value-shaped ones; a bare word is only a secret in context, which the adapters route before writing
        for x in secrets.iter().filter(|x| scan_value(x).is_some()) {
            let e = guard(
                "f",
                format!("{{\"a\":{{\"b\":[\"use {x} now\"]}}}}").as_bytes(),
            )
            .unwrap_err();
            assert!(e.contains("$.a.b[0]") && !e.contains(x), "{e}");
            assert!(
                guard("f.yaml", format!("a: [\"{x}\"]\n").as_bytes()).is_err(),
                "{x}"
            );
            assert!(guard("f", format!("# t\n\nx {x}\n").as_bytes())
                .unwrap_err()
                .contains("line 3"));
        }
        let ok =
            "{\"d\":\"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",\
\"id\":\"0f8fad5b-d9cb-469f-a165-70867728950e\"}";
        assert_eq!(guard("f", ok.as_bytes()), Ok(()));
    }

    #[test]
    fn every_seeded_corpus_line_is_suspicious() {
        let secrets: Vec<&str> = SEEDED_SECRETS.lines().filter(|l| !l.is_empty()).collect();
        let mut n = 0;
        for l in SEEDED.lines() {
            if secrets.iter().any(|s| l.contains(s)) {
                flagged(l);
                n += 1;
            }
        }
        assert!(n > 60, "{n}");
    }

    /// Known evasions found in review (synthetic values).
    #[test]
    fn known_evasions_are_suspicious() {
        for t in [
            // further cases
            "STRIPE_KEY in vault, value is pa55w0rdXYZ",
            "DB_PASS: Tr0ub4dor&3",
            "DB_PASS2 = Tr0ub4dor3x",
            "VAULT_PW: \"correct horse battery staple\"",
            "API_TOKEN (in ~/.env: abcd1234efgh)",
            "OPENAI in vault — sk-proj-QVXabcdefghijkl",
            "`ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789`",
            "AKIAIOSFODNN7EXAMPL3",
            "MY_KEY at ~/.env:XyZ9988776655",
            "SLACK_TOKEN in xoxp-1234-5678-abcdefgh",
            "MCP server at https://user:Pa55wordQQ99@notes.example.test/mcp", // trufflehog:ignore (fake)
            "https://crm.example.test/mcp/T0KENVALUE1234abcd",
            "connect with password=QVXvalue",
            "https://c.example.test/mcp?api_key=QVXv",
            // further cases
            "https://h.example.test/mcp#access_token=QVXfragtokenvalue",
            "https://h.example.test/mcp;jsessionid=QVXv",
            "Log in with password QVXloginsecretvalue and check",
            "Send header Authorization: Bearer QVXbearerlongvalue",
            "token QVXtokenwordvalue",
            "API_KEY=Qv9env",
            "built-in, password QVXbuiltinpassword",
            // further cases
            "access_token=V1 x",
            "news, private_key=AUDprivkey12",
            "client_secret V2",
            "db_password V3 ok",
            "my_token: V4",
            "aws_secret_access_key V5",
            "https://an.example.test/mcp%3Ftoken%3DAUDpct23",
            "PIN 4821 then",
            "The credentials are V",
            "Password：V",
            // further cases
            "sessionToken=AUDr18x",
            "clientSecret: V",
            "DbPassword=V",
            "refresh-token=V",
            "Private-Key: V",
            "GITHUB_PAT=V",
            "x-api-key: V",
            "apiKey=V",
            "db.password=V",
            "pin=1234",
            "SECRET-KEY=V",
            "oauth_token=V",
            "client-secret: V",
            "TOKEN=V",
            "passphrase AUDr20x",
            "the password: \"AUDr15x\"",
            "redis://:QVXv@cache.example.test:6379",
            "postgres://app:QVXv@db.example.test/app", // trufflehog:ignore (fake)
            // further cases
            "password=<zqhunter>",
            "password={hunter2}",
            "password: [hunter2x]",
            "password is: x",
            "password -> x",
            "password → x",
            "Password - x",
            "password was set to x",
            "passcode: x",
            "use the passphrase x",
            "pass phrase is x",
            "ｐａｓｓｗｏｒｄ＝ｈｕｎｔｅｒ２",
            "ＡＰＩ＿ＫＥＹ＝Ｑｖ９ｅｎｖ",
        ] {
            flagged(t);
        }
    }

    #[test]
    fn urls_query_fragment_matrix_userinfo_and_percent_encoded() {
        for u in [
            "https://u:p@h.example.test/mcp",
            "https://u@h.example.test/mcp",
            "https://h.example.test/mcp?token=abc",
            "https://h.example.test/mcp?x=1&sig=abc",
            "https://h.example.test/mcp?code=abc",
            "https://h.example.test/mcp#access_token=abc",
            "https://h.example.test/mcp#QVXbarefragvalue",
            "https://h.example.test/mcp;sessionid=abc",
            "https://h.example.test/mcp;jsessionid=abc",
            "https://h.example.test/key/abcdef/mcp",
            "https://h.example.test/token/QVXsegtokenvalue/mcp",
            "https://h.example.test/v1/sk-a1B2c3D4e5F6g7H8i9J0/mcp",
            "https://h.example.test/mcp%3Ftoken%3Dabc",
            "https://h.example.test/mcp%23access%3Dabc",
            "https://h.example.test/mcp%3Bsid%3Dabc",
            "https://u%3Ap%40h.example.test@h.example.test/",
            "https://h.example.test/mcp?API-KEY=abc",
            "https://h.example.test/mcp?accessToken=abc",
        ] {
            flagged(u);
        }
        for u in [
            "https://demo.example.test/mcp",
            "https://h.example.test/mcp?q=1#top",
            "http://h.example.test:12345/mcp",
            "https://h.example.test/a%20b/mcp",
            "https://h.example.test/mcp?page=2&sort=date",
        ] {
            assert_eq!(scan(u), None, "{u}");
        }
        // `api` + `keys` run together across a separator: errs suspicious.
        flagged("https://h.example.test/api/keys");
    }

    #[test]
    fn token_shapes() {
        let fake = |p: &str, n: usize| {
            format!("{p}{}", "Ab3".repeat(n).chars().take(n).collect::<String>())
        };
        for t in [
            fake("sk-", 32),
            fake("sk-ant-", 32),
            fake("xai-", 24),
            fake("ghp_", 36),
            fake("gho_", 36),
            fake("github_pat_", 30),
            fake("glpat-", 20),
            "xoxb-1234-5678-abcdefgh".to_string(),
            format!("AKIA{}", "ABCDEFGHIJKLMNOP"),
            format!("Bearer {}", fake("", 12)),
            format!("eyJ{}.eyJ{}.{}", fake("", 10), fake("", 12), fake("", 8)),
            format!("-----BEGIN RSA {}-----", "PRIVATE KEY"),
            "use sk-Zq7Wv3Xk9Lm2Np8Rt4Yb here".into(),
            "value pa55w0rdXYZ".into(),
        ] {
            flagged(&t);
        }
    }

    #[test]
    fn prose_phrasing() {
        for t in [
            "the password is hunter",
            "my secret is xyz.",
            "the token is QVXv",
            "the SMTP password is swordfish",
            "auth header QVXauthheadervalue",
            "our API key: QVXv",
            "Authorization: Basic QVXv=",
            "cookie: QVXcookievalueone",
            "session QVXsessionvalueone",
        ] {
            flagged(t);
        }
    }

    /// Any secret keyword, as a word or word part in any separator form, routes the text, with the keyword named.
    #[test]
    fn secret_keywords_are_flagged_with_the_keyword() {
        for (t, k) in [
            ("token economy", "token"),
            ("password reset email copy", "password"),
            ("token budget", "token"),
            ("max_tokens=500", "token"),
            ("tokenize the text", "token"),
            ("call the secretary", "secret"),
            ("PGPASSWORD=${PGPASSWORD_FROM_ENV}", "password"),
            ("password=<YOUR_PASSWORD>", "password"),
            ("update the password policy", "password"),
            ("passwd file", "passwd"),
            ("the pwd", "pwd"),
            ("userPwd", "pwd"),
            ("passphrase", "passphrase"),
            ("passcode", "passcode"),
            ("api key", "api key"),
            ("API-Key", "api key"),
            ("apiKey", "api key"),
            ("bearer auth", "bearer"),
            ("the credentials", "credential"),
            ("private key", "private key"),
            ("ssh_key", "ssh key"),
            ("sshKey", "ssh key"),
            ("send the OTP", "otp"),
            ("sendOtp", "otp"),
            ("otp_code", "otp"),
            ("enable 2FA", "2fa"),
            ("the pin code", "pin code"),
            ("PinCode", "pin code"),
            ("ｔｏｋｅｎ", "token"),
        ] {
            let r = scan(t).unwrap_or_else(|| panic!("not flagged: {t:?}"));
            assert_eq!(
                r.0,
                format!("mentions a secret keyword ('{k}'); review and add by hand if it holds no secret"),
                "{t:?}"
            );
        }
    }

    /// Without a keyword (and no url/token shape) a prompt is clean. `key` alone is not a keyword; `otp`, `2fa`,
    /// `pwd` must be a whole word part and `pin code` a phrase, so these ordinary words do not collide.
    #[test]
    fn prompts_without_keywords_are_not_flagged() {
        for t in [
            "summarise the key points",
            "the key to success",
            "send me the weather at 7",
            "review open PRs",
            "Summarise the sample log",
            "key points of the day",
            "pass the results to the team",
            "batch_size=10",
            "Check the pings every 15 min",
            "the key is to focus",
            "Key: the main point",
            "fetch https://feed.example.test/rss and summarise",
            "hotpot recipes",
            "check the output",
            "laptop deals",
            "spinach and pine nuts",
            "the code review",
        ] {
            assert_eq!(scan(t), None, "{t:?}");
        }
    }

    fn style(words: &[&str], s: usize) -> String {
        let cap = |w: &str| {
            let mut c = w.chars();
            c.next().map_or(String::new(), |f| {
                f.to_uppercase().collect::<String>() + c.as_str()
            })
        };
        match s {
            0 => words.join("_"),
            1 => words.join("-"),
            2 => words.join("."),
            3 => words.join("_").to_uppercase(),
            4 => words.join("-").to_uppercase(),
            5 => words[..1]
                .iter()
                .map(|w| w.to_string())
                .chain(words[1..].iter().map(|w| cap(w)))
                .collect(),
            6 => words.iter().map(|w| cap(w)).collect(),
            _ => words
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    if i % 2 == 0 {
                        w.to_uppercase()
                    } else {
                        w.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("_"),
        }
    }

    /// Generated `key=value` / `key: value` / `key is value` over secret words, case styles and affixes.
    #[test]
    fn generated_credential_assignments_are_suspicious() {
        let bases: &[&[&str]] = &[
            &["password"],
            &["passwd"],
            &["pwd"],
            &["pin"],
            &["token"],
            &["secret"],
            &["passphrase"],
            &["api", "key"],
            &["auth", "token"],
            &["client", "secret"],
            &["db", "password"],
            &["x", "api", "key"],
            &["access", "key"],
            &["private", "key"],
            &["refresh", "token"],
            &["session", "token"],
            &["apikey"],
            &["credentials"],
            &["bearer"],
            &["cookie"],
        ];
        let prefixes: &[&[&str]] = &[&[], &["my"], &["prod"], &["github"], &["aws", "secret"]];
        let suffixes: &[&[&str]] = &[&[], &["2"], &["value"], &["v1"]];
        let forms = [
            "{k}={v}",
            "{k} = {v}",
            "{k}: {v}",
            "{k}:{v}",
            "\"{k}\": \"{v}\"",
            "{k} is {v}",
            "the {k} is {v}",
            "{k}=\"{v}\"",
            "{k}：{v}",
        ];
        let mut n = 0u32;
        for b in bases {
            for p in prefixes {
                for sfx in suffixes {
                    let words: Vec<&str> = p
                        .iter()
                        .chain(b.iter())
                        .chain(sfx.iter())
                        .copied()
                        .collect();
                    for s in 0..8 {
                        let k = style(&words, s);
                        for f in forms {
                            n += 1;
                            let v = format!("Qx{n}v");
                            let t =
                                format!("before {} after", f.replace("{k}", &k).replace("{v}", &v));
                            assert!(scan(&t).is_some(), "not flagged: {t:?}");
                        }
                    }
                }
            }
        }
        assert!(n > 10_000, "{n}");
    }

    /// Regressions (synthetic values): keywords split by any separator, zero-width and homoglyph evasions.
    #[test]
    fn split_keywords_and_unicode_evasions_are_suspicious() {
        for t in [
            "Unlock the vault with Pass-Phrase zqhorsebattery and summarise",
            "pass_phrase zq",
            "pass.phrase zq",
            "the pass code is zq",
            "pass word: zq",
            "Pass Word zq",
            "use the api-Key",
            "Pin Code zq",
            "pa\u{200B}ssword=zqhunter",
            "pass\u{00AD}word zq",
            "to\u{2060}ken zq",
            "\u{0440}\u{0430}ssword=hunter",
            "sum\u{043C}arise",
        ] {
            flagged(t);
        }
        for t in [
            "summarise the key points",
            "Résumé of the café news",
            "Новости дня",
        ] {
            assert!(scan(t).is_none(), "{t:?}: {:?}", scan(t));
        }
        assert!(scan("раssword").unwrap().0.contains("mixed-script"));
    }

    #[test]
    fn scan_value_catches_a_bare_keyword_value() {
        for t in [
            "my password hunter",
            "passphrase zqhorse",
            "Passcode: zq",
            "passwd zq",
        ] {
            assert!(scan_value(t).is_some(), "{t:?}");
        }
        assert!(scan_value("set the password in the vault").is_none());
    }

    #[test]
    fn credential_names_are_plain_identifiers_without_a_value() {
        for n in [
            "VAULT_PW",
            "DB_PASS",
            "TELEGRAM_BOT_TOKEN",
            "openai_api_key",
            "DEMO_TOKEN",
            "smtp-login",
            "Stripe",
            "GitHubToken",
        ] {
            assert_eq!(credential_name(n), None, "{n}");
        }
        for n in [
            "PW_zqhunterpass",
            "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
            "bad name",
            "1ABC",
            "x=y",
            "db_zqtrhunter",
        ] {
            assert!(credential_name(n).is_some(), "{n}");
        }
    }

    #[test]
    fn display_keeps_only_plain_leading_words() {
        let d = display("Log in with password QVXloginsecretvalue and check");
        assert!(!d.contains("QVX"), "{d}");
        assert!(d.starts_with("Log in with"), "{d}");
        assert!(!display("Zq7Wv3Xk9Lm2Np8Rt4Yb").contains("Zq7"));
        assert!(!display("API_KEY=Qv9env").contains("Qv9env"));
    }
    /// Regressions (synthetic values).
    /// The tool's own skill list (`aws-secrets-manager` next to a slug) and paths are not values; a
    /// bare keyword-free random word is a documented limit (docs/SECURITY.md), only a value position is judged.
    #[test]
    fn round_14_value_positions_only() {
        assert_eq!(
            guard_str("  skills: create aws-secrets-manager aws-sdk-v3-usage aws-q"),
            None
        );
        assert_eq!(
            guard_str("details: /var/tmp.286U0Niu1x/out.7Gq2Xk9PzW/d.json"),
            None
        );
        assert_eq!(scan("use Zq7Wv3Xk9Lm2Np8Rt4Yb here"), None);
        assert_eq!(
            scan("https://h.example.test/v1/a1B2c3D4e5F6g7H8i9J0/mcp"),
            None
        );
        flagged("login: Zq7Wv3Xk9Lm2Np8Rt4Yb");
        flagged("the code is Zq7Wv3Xk9Lm2Np8Rt4Yb");
        flagged("https://h.example.test/mcp?x=Zq7Wv3Xk9Lm2Np8Rt4Yb");
        flagged("my secret aws-sdk-key-q");
        // NFKC and zero-width: full-width / ligature / zero-width-split keywords still match
        flagged("ｐａｓｓｗｏｒｄ hunter2x");
        flagged("pass\u{200B}word: x");
    }

    #[test]
    fn round_12_spaced_pass() {
        for t in ["login p a s s zqbb2two", "wifi p a s s zqm1mem"] {
            flagged(t);
        }
        assert_eq!(scan("a b c then go"), None);
    }
    /// Regressions (synthetic values).
    #[test]
    fn round_11_repros() {
        // m1: leet and spaced letters
        for t in [
            "use p@ssword zqbbtwo",
            "use p4$$w0rd zq",
            "my 7ok3n zq",
            "pass w o r d zqgg",
            "p a s s w o r d zq",
            "s e c r e t zq",
            // m2: pw, creds, pin as whole word parts
            "log in with pw zqaaone",
            "use creds zqkkeleven",
            "the PIN zq",
        ] {
            flagged(t);
        }
        for t in [
            "summarise the key points",
            "output to the laptop in 2024",
            "spin up a pinned tab at 9am",
            "a b c then go",
        ] {
            assert_eq!(scan(t), None, "{t}");
        }
        // m2: connector urls
        for u in ["https://h.test/mcp#zqnnfrag", "https://h.test/mcp?k=zqoo"] {
            assert!(connector_url(u).is_some(), "{u}");
        }
        for u in [
            "https://demo.example.test/mcp?x=1",
            "https://demo.example.test/mcp",
        ] {
            assert_eq!(connector_url(u), None, "{u}");
        }
    }

    /// Rule (A), routing: keyword-based and conservative, for schedule prompts/fields and credential names.
    #[test]
    fn rule_a_routing_table() {
        for t in [
            "rotate the API token weekly",
            "api_key=abc123def456",
            "send the p@ssword to me",
            "use my s e c r e t",
            "the pin code",
            "https://u:p@h.example.test/x",
        ] {
            assert!(scan(t).is_some(), "A should route: {t:?}");
        }
        for t in [
            "summarise the key points",
            "post the morning digest at 9",
            "check the output of the build",
        ] {
            assert_eq!(scan(t), None, "{t:?}");
        }
        assert!(credential_name("PW_zqhunterpass").is_some());
        assert_eq!(credential_name("OPENAI_API_KEY"), None);
    }

    /// Rule (B), the write guard: high precision, no keyword rule; a hit names the field, never the value.
    #[test]
    fn rule_b_write_guard_table() {
        for t in [
            "api_key=abc123def456",
            "pass: hunter2x",
            "pin: 4821",
            "token: zqvalue",
            "\"client_secret\": \"zqTwo-word9\"",
            "PROVIDER=sk-0123456789abcdef0123456789",
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012",
            "see https://bob:zqpw@h.example.test/x", // trufflehog:ignore (fake)
            "https://h.example.test/mcp?access_token=zqval",
            "https://h.example.test/mcp;sig=zqval",
            "https://h.example.test/mcp#key=zqval",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
        ] {
            assert!(guard_str(t).is_some(), "B should refuse: {t:?}");
        }
        for t in [
            "rotate the API token weekly",
            "the CRM Postgres password lives in .env",
            "12 pass / 0 fail",
            "api_key: ${OPENROUTER_API_KEY}",
            "PGPASSWORD=${PGPASSWORD_FROM_ENV}",
            "token: ",
            "max_tokens: 4096",
            "OPENAI_API_KEY",
            "https://demo.example.test/mcp?x=1",
            "summarise the key points",
            "PASS: 17/17, FAIL: 0/17",
            "battery pass: 12/12",
            "dossier-pass: review",
            "session_id: a1b2c3d",
            "client_id=abc123def456",
            "name: some-sdk-v3-usage",
            "name: text-to-csv2-table-pack",
            "login: Zq7Wv3Xk9Lm2Np8Rt4Yb",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "id: a1B2c3D4e5F6g7H8i9J0",
        ] {
            assert_eq!(guard_str(t), None, "{t:?}");
        }
        let e = guard("f.json", br#"{"a":"api_key=abc123def456"}"#).unwrap_err();
        assert!(e.contains("$.a") && !e.contains("abc123"), "{e}");
        assert_eq!(guard("f.json", br#"{"a":"token zqlower"}"#), Ok(()));
        assert_eq!(guard("f.md", b"rotate the API token weekly\n"), Ok(()));
        let e = guard("f.md", b"x\napi_key=abc123def456\n").unwrap_err();
        assert!(e.contains("line 2") && !e.contains("abc123"), "{e}");
        let reason = scan("token zq").unwrap().to_string();
        assert_eq!(
            guard(
                "f.json",
                serde_json::json!({"r": reason, "n": "OPENAI_API_KEY", "max_tokens": 5})
                    .to_string()
                    .as_bytes()
            ),
            Ok(())
        );
        let e = guard_args(
            "cron create",
            &[("name", "Nova: B"), ("prompt", "use api_key=abc123def456")],
        )
        .unwrap_err();
        assert!(e.contains("prompt") && !e.contains("abc123"), "{e}");
        assert_eq!(
            guard_args(
                "cron create",
                &[
                    ("name", "Nova: B"),
                    ("schedule", "0 9 * * *"),
                    ("prompt", "rotate the API token weekly")
                ]
            ),
            Ok(())
        );
    }
}

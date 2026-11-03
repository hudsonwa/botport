//! Connector mapping between the Grok Bot and Hermes (docs/design/MIRROR.md section 10). Public, off-the-shelf
//! connectors map through the compiled-in table `connectors.toml` by normalised name; everything else is carried as
//! is: an app with an MCP url keeps its id and url (under the url policy below), an app with neither becomes an
//! opaque record of allowlisted fields, sent back unchanged. No connector outside the table is special-cased.
//!
//! Url policy: https only; no userinfo and no credential-like query ([`secrets::connector_url`]); a private,
//! loopback or link-local host only when the receiving side already has that exact url configured.
use crate::adapters::grok_bot::export_json::app_id;
use crate::core::secrets;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;

/// The fields an opaque record keeps; anything else in the app record is dropped, never carried.
pub const OPAQUE_FIELDS: [&str; 5] = ["id", "name", "kind", "url", "tools"];

/// The Hermes side of a table row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HermesSide {
    /// The vendor's official remote MCP server: an `mcp_servers.<id>` entry with this url.
    Mcp {
        /// Official endpoint, from the vendor's own docs (the row's `source`).
        mcp_url: String,
        /// How it signs in: `oauth`, or `oauth-or-bearer` when a token header also works.
        auth: String,
    },
    /// No verified official endpoint: what the owner does by hand.
    Manual {
        /// The instruction for the manual checklist.
        manual: String,
    },
}

/// One public connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// Canonical id, also the Hermes `mcp_servers` key.
    pub id: String,
    /// The Grok app's display names; the first is the one written back.
    pub grok: Vec<String>,
    /// The Hermes side.
    pub hermes: HermesSide,
    /// Credential names the Hermes side needs (names only).
    #[serde(default)]
    pub credentials: Vec<String>,
    /// The vendor page the endpoint was checked against.
    pub source: String,
}

impl Row {
    /// The official endpoint, when the row has one.
    pub fn mcp_url(&self) -> Option<&str> {
        match &self.hermes {
            HermesSide::Mcp { mcp_url, .. } => Some(mcp_url),
            HermesSide::Manual { .. } => None,
        }
    }

    /// The Grok app name written back for this row.
    pub fn grok_name(&self) -> &str {
        &self.grok[0]
    }

    /// The `mcp_servers` entry for this row as (key, entry), or None for a manual row.
    pub fn hermes_entry(&self) -> Option<(String, Value)> {
        self.mcp_url()
            .map(|u| (self.id.clone(), json!({ "url": u })))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    connector: Vec<Row>,
}

/// Parse a table; a row with no Grok name, a duplicate id or a name two rows share is an error.
fn parse(text: &str) -> Result<Vec<Row>, String> {
    let t: Table = toml::from_str(text).map_err(|e| format!("connectors.toml: {e}"))?;
    let mut names = BTreeSet::new();
    for r in &t.connector {
        if r.grok.is_empty() {
            return Err(format!("connectors.toml: {} has no grok name", r.id));
        }
        let mut own = BTreeSet::new();
        for n in std::iter::once(&r.id).chain(&r.grok) {
            own.insert(norm(n));
        }
        for n in own {
            if !names.insert(n.clone()) {
                return Err(format!("connectors.toml: name {n:?} is in two rows"));
            }
        }
    }
    Ok(t.connector)
}

/// The compiled-in table.
pub fn table() -> &'static [Row] {
    static T: OnceLock<Vec<Row>> = OnceLock::new();
    T.get_or_init(|| parse(include_str!("connectors.toml")).expect("connectors.toml parses"))
}

/// A name for matching: lowercase letters and digits only (case, spaces and punctuation ignored).
pub fn norm(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The row whose id or Grok name matches `name`, or the part of it before a `(`, `;` or ` - ` (apps often carry a
/// description there).
pub fn find(name: &str) -> Option<&'static Row> {
    let head = name.split(['(', ';']).next().unwrap_or(name);
    let head = head.split(" - ").next().unwrap_or(head);
    let keys = [norm(name), norm(head)];
    table().iter().find(|r| {
        std::iter::once(&r.id)
            .chain(&r.grok)
            .any(|n| keys.iter().any(|k| !k.is_empty() && *k == norm(n)))
    })
}

/// What one connector becomes on the receiving side.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "as", rename_all = "kebab-case")]
pub enum Mapping {
    /// A public connector from the table.
    Mapped {
        /// The table row.
        row: &'static Row,
    },
    /// Carried as is: its id and MCP url.
    CarriedUrl {
        /// Short id.
        id: String,
        /// The url, unchanged.
        url: String,
    },
    /// Carried as is: the allowlisted fields of the record, unchanged.
    Opaque {
        /// `id`, `name`, `kind`, `url`, `tools` when present, values as given.
        fields: BTreeMap<String, Value>,
    },
    /// Not carried: its url breaks the url policy (the reason never quotes the url).
    Refused {
        /// The connector's name.
        name: String,
        /// Why.
        reason: String,
    },
}

/// True for a host that is not on the public internet: `localhost`, a single-label or `.local`/`.localhost`/
/// `.internal` name, or a loopback, private, link-local, unique-local, CGNAT or unspecified address.
pub fn private_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let h = h.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = h.parse::<IpAddr>() {
        return private_ip(ip);
    }
    // the forms a system resolver reads as an address although Rust's parser does not (127.1, 0x7f.1, 0177.0.0.1,
    // a bare 2130706433): any all-numeric or hex label set is treated as an address, and refused unless public
    if let Some(v4) = loose_ipv4(h) {
        return private_v4(v4);
    }
    !h.contains('.')
        || [".localhost", ".local", ".internal", ".lan", ".home.arpa"]
            .iter()
            .any(|s| h.ends_with(s))
}

/// An IPv4 address the way `inet_aton` reads one: 1 to 4 parts, each decimal, octal (leading 0) or hex (0x), the last
/// part filling the remaining bytes. None when the host is not of that shape.
fn loose_ipv4(h: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = h.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let num = |s: &str| -> Option<u64> {
        if let Some(x) = s.strip_prefix("0x") {
            u64::from_str_radix(if x.is_empty() { "0" } else { x }, 16).ok()
        } else if s.len() > 1 && s.starts_with('0') {
            u64::from_str_radix(&s[1..], 8).ok()
        } else {
            s.parse().ok()
        }
    };
    let v: Vec<u64> = parts.iter().map(|p| num(p)).collect::<Option<_>>()?;
    let (last, head) = v.split_last()?;
    if head.iter().any(|x| *x > 255) {
        return None;
    }
    let width = 8 * (4 - head.len() as u32);
    if *last >= 1u64 << width {
        return None;
    }
    let mut n: u64 = 0;
    for x in head {
        n = n << 8 | x;
    }
    n = n << width | last;
    Some(Ipv4Addr::from(n as u32))
}

fn private_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return private_v4(v4);
            }
            let seg = v6.segments();
            let s = seg[0];
            // IPv4-compatible (::a.b.c.d), NAT64 (64:ff9b::/96) and 6to4 (2002::/16) carry an IPv4 address inside
            let inner = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
            let embedded = if (seg[..6] == [0, 0, 0, 0, 0, 0] && !(seg[6] == 0 && seg[7] <= 1))
                || seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
            {
                Some(inner(seg[6], seg[7]))
            } else if s == 0x2002 {
                Some(inner(seg[1], seg[2]))
            } else {
                None
            };
            if embedded.is_some_and(private_v4) {
                return true;
            }
            v6 == Ipv6Addr::LOCALHOST
                || v6.is_unspecified()
                || v6.is_multicast()
                // Teredo (2001::/32) hides an IPv4 address; local-use NAT64 (64:ff9b:1::/48); discard (100::/64)
                || (s == 0x2001 && seg[1] == 0)
                || (s == 0x64 && seg[1] == 0xff9b && seg[2] == 1)
                || (s == 0x100 && seg[1..4] == [0, 0, 0])
                || (s & 0xffc0) == 0xfe80
                || (s & 0xffc0) == 0xfec0
                || (s & 0xfe00) == 0xfc00
        }
    }
}

/// The host part of an https url (an IPv6 literal without its brackets), if it has one.
fn url_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    (!host.is_empty()).then_some(host)
}

/// The system resolver: every address `host` resolves to (none when it does not resolve). Names under the reserved
/// `.test`, `.example` and `.invalid` domains never resolve publicly and are not looked up.
pub fn system_resolve(host: &str) -> Vec<IpAddr> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if [".test", ".example", ".invalid"]
        .iter()
        .any(|s| h.ends_with(s))
    {
        return vec![];
    }
    use std::net::ToSocketAddrs;
    (h.as_str(), 443)
        .to_socket_addrs()
        .map(|a| a.map(|a| a.ip()).collect())
        .unwrap_or_default()
}

/// Resolve, then check, right before a connector is written: refused when its host does not resolve or resolves to
/// any private, loopback or link-local address, unless the receiving side already has that url. The platform resolves
/// the name again when it connects; what the name resolves to then is a residual risk (design section 10).
pub fn check_resolved(
    url: &str,
    configured: &BTreeSet<String>,
    resolve: impl Fn(&str) -> Vec<IpAddr>,
) -> Result<(), String> {
    check_url(url, configured)?;
    let Some(host) = url_host(url) else {
        return Ok(());
    };
    if configured.contains(url) || host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let addrs = resolve(host);
    if addrs.is_empty() {
        return Err("the host does not resolve (checked again once it does)".into());
    }
    if addrs.into_iter().any(private_ip) {
        return Err(
            "the host resolves to a private, loopback or link-local address not already configured on the receiving side"
                .into(),
        );
    }
    Ok(())
}

/// The url policy: Ok, or why the url is refused. `configured` holds the urls the receiving side already has.
pub fn check_url(url: &str, configured: &BTreeSet<String>) -> Result<(), String> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err("not an https url".into());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err("url with userinfo (user:password@host)".into());
    }
    // what a browser-style parser would read differently from a plain one: refused rather than guessed
    if rest.contains('\\') || authority.contains('%') {
        return Err("url with a backslash or an encoded host".into());
    }
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']');
    if !authority.chars().all(plain) {
        return Err(
            "url host has characters outside a-z, 0-9, '.', '-' (use the ASCII form of a name)"
                .into(),
        );
    }
    if let Some(r) = secrets::connector_url(url) {
        return Err(r.to_string());
    }
    // the host, without a port (an IPv6 literal keeps its colons inside brackets)
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    if host.is_empty() {
        return Err("url without a host".into());
    }
    if private_host(host) && !configured.contains(url) {
        return Err(
            "private, loopback or link-local host not already configured on the receiving side"
                .into(),
        );
    }
    Ok(())
}

/// A string field of a record, trimmed; empty counts as absent.
fn field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A Grok app record (`name`, optional `id`, `mcp_url` or `url`, ...) -> what it becomes on the Hermes side.
/// `configured`: the urls already in the Hermes profile's `mcp_servers`. A table row maps when the app has no url or
/// the row's own; an app of a listed name with another url is that url, carried as is.
pub fn from_grok(app: &Value, configured: &BTreeSet<String>) -> Mapping {
    let name = field(app, "name").unwrap_or("");
    // a url that is there but not a string is not quietly dropped: the record is refused
    for k in ["mcp_url", "url"] {
        if app.get(k).is_some_and(|v| !v.is_string() && !v.is_null()) {
            return Mapping::Refused {
                name: secrets::display(name),
                reason: format!("{k} is not a string"),
            };
        }
    }
    if app
        .get("tools")
        .is_some_and(|v| !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
    {
        return Mapping::Refused {
            name: secrets::display(name),
            reason: "tools is not a list of names".into(),
        };
    }
    let url = field(app, "mcp_url").or_else(|| field(app, "url"));
    if let Some(row) = find(name) {
        if url.is_none_or(|u| same_url(u, row.mcp_url())) {
            return Mapping::Mapped { row };
        }
    }
    if let Some(u) = url {
        return match check_url(u, configured) {
            Ok(()) => Mapping::CarriedUrl {
                id: field(app, "id").map(app_id).unwrap_or_else(|| app_id(name)),
                url: u.to_string(),
            },
            Err(reason) => Mapping::Refused {
                name: secrets::display(name),
                reason,
            },
        };
    }
    Mapping::Opaque {
        fields: OPAQUE_FIELDS
            .iter()
            .filter_map(|k| app.get(*k).map(|v| (k.to_string(), v.clone())))
            .collect(),
    }
}

/// Equal urls, ignoring one trailing `/`.
fn same_url(a: &str, b: Option<&str>) -> bool {
    b.is_some_and(|b| a.trim_end_matches('/') == b.trim_end_matches('/'))
}

/// The reverse direction: the table row for a Hermes `mcp_servers.<key>` entry (its url is the row's official url,
/// or it has none and the key names the row). None for anything else.
pub fn row_for_hermes(key: &str, entry: &Value) -> Option<&'static Row> {
    match field(entry, "url") {
        Some(u) => table().iter().find(|r| same_url(u, r.mcp_url())),
        None => find(key).filter(|r| r.mcp_url().is_none()),
    }
}

/// The Grok app name for a Hermes `mcp_servers` entry of a mapped row.
pub fn grok_name_for_hermes(key: &str, entry: &Value) -> Option<&'static str> {
    row_for_hermes(key, entry).map(Row::grok_name)
}

/// A Hermes `mcp_servers.<key>` entry -> what it becomes on the Grok side. `configured`: the urls the Grok side
/// already has. An entry with no url (a local command) is an opaque record of its key only: commands are never
/// carried.
pub fn from_hermes(key: &str, entry: &Value, configured: &BTreeSet<String>) -> Mapping {
    if let Some(row) = row_for_hermes(key, entry) {
        return Mapping::Mapped { row };
    }
    match field(entry, "url") {
        Some(u) => match check_url(u, configured) {
            Ok(()) => Mapping::CarriedUrl {
                id: key.to_string(),
                url: u.to_string(),
            },
            Err(reason) => Mapping::Refused {
                name: secrets::display(key),
                reason,
            },
        },
        None => Mapping::Opaque {
            fields: [("id".to_string(), json!(key))].into(),
        },
    }
}

/// The neutral form of a connector, for hashing (design section 3): sorted-key compact JSON of the mapped fields.
/// A mapped row is its id alone (both sides then agree whatever they call it); a carried url its id and url; an
/// opaque record its fields. None for a refused connector (it is not carried).
pub fn neutral(m: &Mapping) -> Option<String> {
    let v = match m {
        Mapping::Mapped { row } => json!({"id": row.id, "table": true}),
        Mapping::CarriedUrl { id, url } => json!({"id": id, "url": url}),
        Mapping::Opaque { fields } => json!({"opaque": fields}),
        Mapping::Refused { .. } => return None,
    };
    Some(serde_json::to_string(&v).expect("json"))
}

/// The manual-checklist line for a connector that needs a human (names only): an opaque record ("carried as is"),
/// a manual table row, or a refused url.
pub fn manual_line(m: &Mapping) -> Option<(String, String)> {
    match m {
        Mapping::Opaque { fields } => {
            let n = fields
                .get("name")
                .or(fields.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)");
            Some((
                format!("connector {}", secrets::display(n)),
                "carried as is (no public mapping and no MCP url): set it up by hand on this side"
                    .into(),
            ))
        }
        Mapping::Mapped { row } => match &row.hermes {
            HermesSide::Manual { manual } => {
                Some((format!("connector {}", row.id), manual.clone()))
            }
            HermesSide::Mcp { .. } if !row.credentials.is_empty() => Some((
                format!("connector {}", row.id),
                format!(
                    "sign in; it needs {} (names only)",
                    row.credentials.join(", ")
                ),
            )),
            HermesSide::Mcp { .. } => None,
        },
        Mapping::Refused { name, reason } => Some((
            format!("connector {name}"),
            format!("not carried: {reason}; add it by hand"),
        )),
        Mapping::CarriedUrl { .. } => None,
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn a_name_that_resolves_to_a_private_address_is_refused_unless_configured() {
        let none = BTreeSet::new();
        let to = |ips: &'static [&'static str]| {
            move |_: &str| {
                ips.iter()
                    .map(|i| i.parse().unwrap())
                    .collect::<Vec<IpAddr>>()
            }
        };
        let url = "https://mcp.example.com/mcp";
        assert!(check_resolved(url, &none, to(&["203.0.113.10"])).is_ok());
        let e = check_resolved(url, &none, to(&[])).unwrap_err();
        assert!(e.contains("does not resolve"), "{e}");
        for private in [
            &["10.0.0.5"][..],
            &["203.0.113.10", "127.0.0.1"],
            &["fe80::1"],
            &["::ffff:192.168.1.2"],
        ] {
            let ips: Vec<IpAddr> = private.iter().map(|i| i.parse().unwrap()).collect();
            let e = check_resolved(url, &none, move |_: &str| ips.clone()).unwrap_err();
            assert!(e.contains("resolves to a private"), "{e}");
        }
        let have = BTreeSet::from([url.to_string()]);
        assert!(check_resolved(url, &have, to(&["10.0.0.5"])).is_ok());
        // the policy itself still applies first
        assert!(check_resolved("http://mcp.example.com/", &none, to(&[])).is_err());
        // reserved test names are never looked up
        assert!(system_resolve("demo.example.test").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn the_table_parses_with_sources_https_endpoints_and_plain_credential_names() {
        let t = table();
        assert!(t.len() >= 7);
        for r in t {
            assert!(r.source.starts_with("https://"), "{}", r.id);
            if let Some(u) = r.mcp_url() {
                assert!(check_url(u, &none()).is_ok(), "{}: {u}", r.id);
            }
            for c in &r.credentials {
                assert!(secrets::credential_name(c).is_none(), "{c}");
                assert!(
                    c.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
                    "{c}"
                );
            }
        }
    }

    #[test]
    fn every_row_round_trips() {
        for r in table() {
            for alias in &r.grok {
                let app = json!({"name": alias});
                assert_eq!(
                    from_grok(&app, &none()),
                    Mapping::Mapped { row: r },
                    "{alias}"
                );
            }
            match r.hermes_entry() {
                Some((key, entry)) => {
                    assert_eq!(key, r.id);
                    assert_eq!(grok_name_for_hermes(&key, &entry), Some(r.grok_name()));
                    assert_eq!(
                        from_hermes(&key, &entry, &none()),
                        Mapping::Mapped { row: r }
                    );
                    // the app named back maps to the same row again
                    let back = json!({"name": r.grok_name(), "mcp_url": entry["url"]});
                    assert_eq!(from_grok(&back, &none()), Mapping::Mapped { row: r });
                }
                None => assert_eq!(grok_name_for_hermes(&r.id, &json!({})), Some(r.grok_name())),
            }
            let n = neutral(&Mapping::Mapped { row: r }).unwrap();
            assert_eq!(n, format!(r#"{{"id":"{}","table":true}}"#, r.id));
        }
    }

    #[test]
    fn names_match_ignoring_case_spaces_and_punctuation() {
        // table-driven: the table is the only place connector names appear
        let id = |n: &str| find(n).map(|r| r.id.as_str());
        for r in table() {
            let want = Some(r.id.as_str());
            assert_eq!(id(&r.id), want);
            for a in &r.grok {
                assert_eq!(id(&a.to_uppercase()), want, "{a}");
                assert_eq!(id(&a.to_lowercase().replace(' ', "_")), want, "{a}");
                assert_eq!(id(&format!(" {a}! ")), want, "{a}");
                // letters spaced out, and a description after the name
                let spaced: String = a.chars().flat_map(|c| [c, ' ']).collect();
                assert_eq!(id(&spaced), want, "{a}");
                assert_eq!(id(&format!("{a} (demo pages)")), want, "{a}");
                assert_eq!(id(&format!("{a} - demo workspace")), want, "{a}");
            }
        }
        assert_eq!(id("example-connector"), None);
        assert_eq!(id(""), None);
        assert_eq!(id("()"), None);
    }

    #[test]
    fn a_manual_row_and_bad_tables() {
        let t = parse(
            r#"[[connector]]
id = "example-connector"
grok = ["Example Connector"]
hermes = { manual = "enable the example toolset by hand" }
source = "https://example.com/docs"
"#,
        )
        .unwrap();
        assert_eq!(t[0].mcp_url(), None);
        assert_eq!(t[0].hermes_entry(), None);
        let m = Mapping::Mapped {
            row: Box::leak(Box::new(t[0].clone())),
        };
        assert_eq!(
            manual_line(&m).unwrap().1,
            "enable the example toolset by hand"
        );
        let dup = r#"[[connector]]
id = "a"
grok = ["Same"]
hermes = { manual = "x" }
source = "https://example.com"
[[connector]]
id = "b"
grok = ["same!"]
hermes = { manual = "x" }
source = "https://example.com"
"#;
        assert!(parse(dup).unwrap_err().contains("two rows"));
        let extra = "[[connector]]\nid = \"a\"\ngrok = [\"A\"]\nhermes = { manual = \"x\" }\nsource = \"s\"\ntoken = \"x\"\n";
        assert!(parse(extra).is_err());
        let empty =
            "[[connector]]\nid = \"a\"\ngrok = []\nhermes = { manual = \"x\" }\nsource = \"s\"\n";
        assert!(parse(empty).is_err());
    }

    #[test]
    fn a_url_connector_is_carried_unchanged() {
        let app = json!({"name": "Example Connector (demo)", "mcp_url": "https://mcp.example.com/v1/mcp"});
        assert_eq!(
            from_grok(&app, &none()),
            Mapping::CarriedUrl {
                id: "example-connector".into(),
                url: "https://mcp.example.com/v1/mcp".into()
            }
        );
        let app = json!({"id": "demo-id", "name": "x", "mcp_url": "https://mcp.example.com/mcp?transport=sse"});
        assert!(
            matches!(from_grok(&app, &none()), Mapping::CarriedUrl { id, .. } if id == "demo-id")
        );
        // a listed name with someone else's url is that url, not the public row
        let row = table().iter().find(|r| r.mcp_url().is_some()).unwrap();
        let app = json!({"name": row.grok_name(), "mcp_url": "https://mcp.example.com/demo"});
        assert!(matches!(
            from_grok(&app, &none()),
            Mapping::CarriedUrl { .. }
        ));
        // the row's own url (with or without the trailing slash) maps
        let own = row.mcp_url().unwrap();
        let other = match own.strip_suffix('/') {
            Some(u) => u.to_string(),
            None => format!("{own}/"),
        };
        let app = json!({"name": row.grok_name(), "mcp_url": other});
        assert!(matches!(from_grok(&app, &none()), Mapping::Mapped { .. }));
        let e =
            json!({"url": "https://mcp.example.com/v1/mcp", "tools": {"include": ["demo_tool"]}});
        assert_eq!(
            from_hermes("example-connector", &e, &none()),
            Mapping::CarriedUrl {
                id: "example-connector".into(),
                url: "https://mcp.example.com/v1/mcp".into()
            }
        );
        assert_eq!(grok_name_for_hermes("example-connector", &e), None);
        assert_eq!(
            neutral(&from_hermes("example-connector", &e, &none())).unwrap(),
            r#"{"id":"example-connector","url":"https://mcp.example.com/v1/mcp"}"#
        );
    }

    #[test]
    fn hosts_a_resolver_reads_as_private_addresses_are_refused() {
        for u in [
            "https://127.1/mcp",
            "https://0x7f.0.0.1/mcp",
            "https://0177.0.0.1/mcp",
            "https://2130706433/mcp",
            "https://10.1/mcp",
            "https://[::7f00:1]/mcp",
            "https://[64:ff9b::a00:1]/mcp",
            "https://[2002:c0a8:101::]/mcp",
            "https://[fec0::1]/mcp",
            "https://%31%32%37.0.0.1/mcp",
            "https://127.0.0.1\\x.example.com/mcp",
            "https://\u{ff11}\u{ff12}\u{ff17}.0.0.1/mcp",
            "https://198.18.0.1/mcp",
            "https://240.0.0.1/mcp",
            "https://224.0.0.251/mcp",
            "https://192.0.0.8/mcp",
            "https://[2001:0:4136:e378::1]/mcp",
            "https://[ff02::1]/mcp",
            "https://[64:ff9b:1::a00:1]/mcp",
            "https://[100::1]/mcp",
        ] {
            assert!(check_url(u, &none()).is_err(), "{u}");
        }
        // public addresses in those forms are still addresses, and public
        assert!(check_url("https://8.8.8.8/mcp", &none()).is_ok());
        assert!(check_url("https://mcp.example.com/mcp", &none()).is_ok());
    }

    #[test]
    fn records_with_odd_url_or_tools_fields_are_refused() {
        let odd = json!({"name": "x", "url": ["https://u:p@10.0.0.1"]});
        assert!(matches!(from_grok(&odd, &none()), Mapping::Refused { .. }));
        let odd = json!({"name": "x", "tools": "all"});
        assert!(matches!(from_grok(&odd, &none()), Mapping::Refused { .. }));
    }

    #[test]
    fn url_policy() {
        let refused = |u: &str| check_url(u, &none()).unwrap_err();
        assert_eq!(refused("http://mcp.example.com/mcp"), "not an https url");
        assert_eq!(refused("ftp://mcp.example.com/mcp"), "not an https url");
        assert!(refused("https://demo:pw@mcp.example.com/mcp").contains("userinfo"));
        assert!(refused("https://mcp.example.com/mcp?api_key=abc123def456").contains("query"));
        assert!(
            refused("https://mcp.example.com/mcp?token=Zq7Wv3Xk9Lm2Np8Rt4Yb").contains("query")
        );
        assert!(refused("https://mcp.example.com/mcp#frag").contains("fragment"));
        assert!(check_url("https://mcp.example.com/mcp?transport=sse", &none()).is_ok());
        // a CGNAT address, built at run time so the literal is not in the source
        let cgnat = format!("https://{}.64.0.1/mcp", 10 * 10);
        for private in [
            cgnat.as_str(),
            "https://localhost/mcp",
            "https://localhost:8443/mcp",
            "https://127.0.0.1/mcp",
            "https://10.0.0.2/mcp",
            "https://172.16.0.1/mcp",
            "https://192.168.0.1:9000/mcp",
            "https://169.254.1.1/mcp",
            "https://0.0.0.0/mcp",
            "https://[::1]/mcp",
            "https://[fe80::1]:8443/mcp",
            "https://[fd00::5]/mcp",
            "https://[::ffff:127.0.0.1]/mcp",
            "https://demo-box/mcp",
            "https://demo.local/mcp",
            "https://demo.internal/mcp",
        ] {
            assert!(refused(private).contains("private"), "{private}");
            // the same url already configured on the receiving side is accepted
            let have: BTreeSet<String> = [private.to_string()].into();
            assert!(check_url(private, &have).is_ok(), "{private}");
        }
        for public in [
            "https://mcp.example.com/mcp",
            "https://203.0.113.7/mcp",
            "https://[2001:db8::1]/mcp",
        ] {
            assert!(check_url(public, &none()).is_ok(), "{public}");
        }
        assert!(check_url("https:///mcp", &none()).is_err());
        // a refused url is never carried, and its reason does not quote it
        let app =
            json!({"name": "Example Connector", "mcp_url": "https://demo:pw@mcp.example.com/mcp"});
        match from_grok(&app, &none()) {
            Mapping::Refused { name, reason } => {
                assert_eq!(name, "Example Connector");
                assert!(!reason.contains("pw@"));
            }
            m => panic!("{m:?}"),
        }
        let local = json!({"name": "Example Connector", "mcp_url": "https://127.0.0.1:8443/mcp"});
        assert!(matches!(
            from_grok(&local, &none()),
            Mapping::Refused { .. }
        ));
        let have: BTreeSet<String> = ["https://127.0.0.1:8443/mcp".to_string()].into();
        assert!(matches!(
            from_grok(&local, &have),
            Mapping::CarriedUrl { .. }
        ));
        assert!(neutral(&from_grok(&app, &none())).is_none());
    }

    #[test]
    fn opaque_records_keep_allowlisted_fields_byte_identical() {
        let raw = r#"{"id":"demo-7","kind":"app","name":"Example Connector","note":"dropped","secret_ref":"dropped","tools":["demo_read","demo_write"],"url":"","extra":{"a":1}}"#;
        let app: Value = serde_json::from_str(raw).unwrap();
        let m = from_grok(&app, &none());
        let Mapping::Opaque { fields } = &m else {
            panic!("{m:?}")
        };
        assert_eq!(
            fields.keys().collect::<Vec<_>>(),
            ["id", "kind", "name", "tools", "url"]
        );
        for (k, v) in fields {
            assert_eq!(v, &app[k], "{k}");
        }
        // sent back: the same allowlisted bytes, again and again
        let once = serde_json::to_string(fields).unwrap();
        assert_eq!(
            once,
            r#"{"id":"demo-7","kind":"app","name":"Example Connector","tools":["demo_read","demo_write"],"url":""}"#
        );
        let again: Value = serde_json::from_str(&once).unwrap();
        let Mapping::Opaque { fields: f2 } = from_grok(&again, &none()) else {
            panic!()
        };
        assert_eq!(serde_json::to_string(&f2).unwrap(), once);
        assert_eq!(
            manual_line(&m).unwrap(),
            (
                "connector Example Connector".to_string(),
                "carried as is (no public mapping and no MCP url): set it up by hand on this side"
                    .to_string()
            )
        );
        assert!(neutral(&m)
            .unwrap()
            .starts_with(r#"{"opaque":{"id":"demo-7""#));
        // a local-command entry on the Hermes side is its key only: the command is never carried
        let e = json!({"command": "demo-server", "args": ["--port", "1"]});
        assert_eq!(
            from_hermes("example-connector", &e, &none()),
            Mapping::Opaque {
                fields: [("id".to_string(), json!("example-connector"))].into()
            }
        );
    }

    #[test]
    fn credentials_needed_are_listed_by_name() {
        for r in table().iter().filter(|r| r.mcp_url().is_some()) {
            let m = from_grok(&json!({"name": r.grok_name()}), &none());
            match (manual_line(&m), r.credentials.is_empty()) {
                (None, true) => {}
                (Some((item, todo)), false) => {
                    assert_eq!(item, format!("connector {}", r.id));
                    for c in &r.credentials {
                        assert!(todo.contains(c.as_str()), "{todo}");
                    }
                }
                (l, _) => panic!("{}: {l:?}", r.id),
            }
        }
    }
}

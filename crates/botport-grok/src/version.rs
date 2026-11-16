//! Tested-versions table and the version-keyed field names the parsers read.
//! The version is probed on connect (debug port: the `GrokBot/x.y.z` User-Agent of `/json/version`).
//! An unknown version: reads run with a warning; writes are refused (exit 4) until `botport grok canary` has
//! passed on that version and transport (preset balanced or fast), or `--trust-version` (fast only).

/// (version, reads tested, writes tested). Add a row only after a live canary and the live suite on a TEST
/// account pass on that version.
pub const TESTED: &[(&str, bool, bool)] = &[("1.1.0", true, true)];

/// The table row for `v`: `Some((reads, writes))`, `None` for a version not in the table.
pub fn lookup(v: &str) -> Option<(bool, bool)> {
    TESTED
        .iter()
        .find(|(t, ..)| *t == v)
        .map(|(_, r, w)| (*r, *w))
}

/// Both reads and writes are tested on `v`.
pub fn is_tested(v: &str) -> bool {
    lookup(v) == Some((true, true))
}

/// The raw field names a version's sidebar agent object must carry. Missing one fails closed (exit 1) naming it.
pub struct Fields {
    pub agent_required: &'static [&'static str],
    pub chat_required: &'static [&'static str],
    pub entry_required: &'static [&'static str],
}

pub const FIELDS_1_1: Fields = Fields {
    agent_required: &[
        "id",
        "name",
        "isRunning",
        "isRunningTurn",
        "isComposingMessage",
        "awaitingUserResponse",
        "lastMessageId",
    ],
    chat_required: &["entries", "isAgentRunning"],
    entry_required: &["kind", "id"],
};

/// The field table for a version. Unknown versions use the newest known table (reads warn; writes gate).
pub fn fields_for(_version: &str) -> &'static Fields {
    &FIELDS_1_1
}

/// `GrokBot/1.1.0` in a User-Agent -> `1.1.0`.
pub fn from_user_agent(ua: &str) -> Option<String> {
    let i = ua.find("GrokBot/")? + "GrokBot/".len();
    let v: String = ua[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ua() {
        assert_eq!(
            super::from_user_agent("Mozilla/5.0 GrokBot/1.1.0 Chrome/150"),
            Some("1.1.0".into())
        );
        assert_eq!(super::from_user_agent("Chrome/150"), None);
        assert!(super::is_tested("1.1.0"));
        assert!(!super::is_tested("1.2.0"));
        assert_eq!(super::lookup("1.1.0"), Some((true, true)));
        assert_eq!(super::lookup("1.0.1"), None);
        assert!(!super::is_tested("unknown"));
    }
}

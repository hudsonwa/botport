//! The pair key: HMAC-SHA256 over mirror files and host triggers. Spec: docs/design/MIRROR.md sections 6.3 and 7.
//!
//! A pair key is 32 random bytes (64 hex characters), kept 0600 on each side. A signed file carries `mac`: the HMAC of
//! its canonical JSON with that field removed. A **trigger** (something on the host asking for a run) carries the
//! pair, the action it asks for, a random nonce, the time and a `mac` over `pair\0action\0nonce\0time`; it is accepted only within [`TRIGGER_WINDOW`]
//! seconds of now and only once per nonce.
//!
//! ```
//! use botport::core::auth::{hmac_sha256, Trigger};
//!
//! assert_eq!(hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog").len(), 64);
//! let key = [7u8; 32];
//! let t = Trigger::new(&key, "demo-bot", "n1", 1_000);
//! let mut seen = vec![];
//! assert!(t.verify(&key, "demo-bot", "sync", 1_100, &mut seen).is_ok());
//! assert!(t.verify(&key, "demo-bot", "sync", 1_100, &mut seen).is_err()); // replayed
//! ```
#![warn(missing_docs)]

use super::util::{err, hex, unhex, Res};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A trigger is accepted only this many seconds either side of now.
pub const TRIGGER_WINDOW: u64 = 300;
/// At most this many nonces are remembered; when full, new triggers are refused until old ones expire (never
/// forgotten early, so a recent trigger can never be replayed).
pub const NONCES_KEPT: usize = 4096;

/// HMAC-SHA256 (RFC 2104) as lowercase hex.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> String {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let mut inner = Sha256::new();
    inner.update(pad(0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(pad(0x5c));
    outer.update(inner.finalize());
    hex(&outer.finalize())
}

/// Compare two strings in time independent of where they differ.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Parse a pair key file's text: exactly 64 hex characters (surrounding whitespace allowed).
pub fn parse_key(t: &str) -> Res<Vec<u8>> {
    let t = t.trim();
    match unhex(t) {
        Some(k) if k.len() == 32 => Ok(k),
        _ => err("a pair key is 64 hex characters"),
    }
}

/// The signature of a digest file in the pair folder (its `<name>.mac` sidecar): HMAC-SHA256 with the pair key over
/// the pair, the file name and the bytes, so a signed digest cannot be moved to another pair or another number.
pub fn digest_mac(key: &[u8], pair: &str, name: &str, text: &[u8]) -> String {
    let mut m = format!("botport-digest-mac/1\n{pair}\n{name}\n").into_bytes();
    m.extend_from_slice(text);
    hmac_sha256(key, &m)
}

/// Whether `sig` (a sidecar's text) is the signature of this digest file.
pub fn digest_mac_ok(key: &[u8], pair: &str, name: &str, text: &[u8], sig: &[u8]) -> bool {
    std::str::from_utf8(sig).is_ok_and(|s| same(s.trim(), &digest_mac(key, pair, name, text)))
}

/// A new pair key's text (from the OS random source).
pub fn new_key() -> Res<String> {
    let mut b = [0u8; 32];
    super::util::os_random(&mut b).map_err(|_| "no random source for a pair key".to_string())?;
    Ok(hex(&b))
}

/// Sign a JSON value: set `mac` to the HMAC of its canonical form without `mac`.
pub fn sign(v: &mut serde_json::Value, key: &[u8]) {
    if let Some(o) = v.as_object_mut() {
        o.remove("mac");
    }
    let m = hmac_sha256(key, super::util::json_sorted(v).as_bytes());
    v["mac"] = serde_json::json!(m);
}

/// Check a signed JSON value (its `mac` must match).
pub fn verify(v: &serde_json::Value, key: &[u8]) -> Res<()> {
    let Some(m) = v["mac"].as_str().map(String::from) else {
        return err("not signed with the pair key");
    };
    let mut c = v.clone();
    if let Some(o) = c.as_object_mut() {
        o.remove("mac");
    }
    if same(
        &hmac_sha256(key, super::util::json_sorted(&c).as_bytes()),
        &m,
    ) {
        Ok(())
    } else {
        err("the pair key signature does not match")
    }
}

/// A host trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    /// The pair it is for.
    pub pair: String,
    /// What it asks for (`sync`); a trigger for one action never runs another.
    pub action: String,
    /// A random nonce, used once.
    pub nonce: String,
    /// Unix seconds when it was made.
    pub time: u64,
    /// HMAC over `pair\0action\0nonce\0time`.
    pub mac: String,
}

impl Trigger {
    fn msg(pair: &str, action: &str, nonce: &str, time: u64) -> Vec<u8> {
        format!("{pair}\0{action}\0{nonce}\0{time}").into_bytes()
    }

    /// A `sync` trigger signed with `key`.
    pub fn new(key: &[u8], pair: &str, nonce: &str, time: u64) -> Trigger {
        Trigger::for_action(key, pair, "sync", nonce, time)
    }

    /// A trigger for `action` signed with `key`.
    pub fn for_action(key: &[u8], pair: &str, action: &str, nonce: &str, time: u64) -> Trigger {
        Trigger {
            pair: pair.into(),
            action: action.into(),
            nonce: nonce.into(),
            time,
            mac: hmac_sha256(key, &Self::msg(pair, action, nonce, time)),
        }
    }

    /// Accept the trigger for `pair` at `now`: right pair, valid mac, within the window, nonce not in `seen`. An
    /// accepted nonce is added to `seen` as `<time>:<nonce>` and kept until its trigger could no longer be accepted.
    pub fn verify(
        &self,
        key: &[u8],
        pair: &str,
        action: &str,
        now: u64,
        seen: &mut Vec<String>,
    ) -> Res<()> {
        if self.pair != pair {
            return err("trigger is for another pair");
        }
        if !same(
            &hmac_sha256(
                key,
                &Self::msg(&self.pair, &self.action, &self.nonce, self.time),
            ),
            &self.mac,
        ) {
            return err("trigger signature does not match");
        }
        if self.action != action {
            return err("trigger asks for another action");
        }
        if self.time.abs_diff(now) > TRIGGER_WINDOW {
            return err("trigger is too old or from the future");
        }
        // forget only nonces whose triggers are outside the window by now
        seen.retain(|s| {
            s.split_once(':')
                .and_then(|(t, _)| t.parse::<u64>().ok())
                .is_some_and(|t| {
                    t.saturating_add(TRIGGER_WINDOW) >= now.saturating_sub(TRIGGER_WINDOW)
                })
        });
        let used = seen
            .iter()
            .any(|s| s.split_once(':').is_some_and(|(_, n)| n == self.nonce));
        if self.nonce.is_empty() || self.nonce.len() > 64 || self.nonce.contains(':') || used {
            return err("trigger nonce was already used (or is invalid)");
        }
        if seen.len() >= NONCES_KEPT {
            return err("too many recent triggers; try again in a few minutes");
        }
        seen.push(format!("{}:{}", self.time, self.nonce));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        assert_eq!(
            hmac_sha256(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // a key longer than the block is hashed first (RFC 4231 case 6)
        assert_eq!(
            hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn signed_values_detect_any_change_and_the_wrong_key() {
        let k = parse_key(&new_key().unwrap()).unwrap();
        let mut v = serde_json::json!({"n": 3, "items": {"skill:demo-skill": "d1"}});
        sign(&mut v, &k);
        assert!(verify(&v, &k).is_ok());
        let mut t = v.clone();
        t["n"] = serde_json::json!(4);
        assert!(verify(&t, &k).is_err());
        assert!(verify(&v, &[0u8; 32]).is_err());
        let mut u = v.clone();
        u.as_object_mut().unwrap().remove("mac");
        assert!(verify(&u, &k).unwrap_err().contains("not signed"));
        assert!(parse_key("abc").is_err());
        assert!(parse_key(&"g".repeat(64)).is_err());
    }

    #[test]
    fn triggers_expire_and_cannot_be_replayed() {
        let k = [1u8; 32];
        let mut seen = vec![];
        let t = Trigger::new(&k, "demo-bot", "nonce-1", 10_000);
        assert!(t.verify(&k, "other", "sync", 10_000, &mut seen).is_err());
        assert!(t
            .verify(&[2u8; 32], "demo-bot", "sync", 10_000, &mut seen)
            .is_err());
        assert!(t
            .verify(
                &k,
                "demo-bot",
                "sync",
                10_000 + TRIGGER_WINDOW + 1,
                &mut seen
            )
            .is_err());
        assert!(t
            .verify(
                &k,
                "demo-bot",
                "sync",
                10_000 - TRIGGER_WINDOW - 1,
                &mut seen
            )
            .is_err());
        assert!(
            seen.is_empty(),
            "rejected triggers do not use up their nonce"
        );
        assert!(t.verify(&k, "demo-bot", "sync", 10_100, &mut seen).is_ok());
        assert!(t.verify(&k, "demo-bot", "sync", 10_100, &mut seen).is_err());
        // a trigger made for another action does not run a sync
        let other = Trigger::for_action(&k, "demo-bot", "plan", "nonce-3", 10_000);
        assert!(other
            .verify(&k, "demo-bot", "sync", 10_000, &mut seen)
            .unwrap_err()
            .contains("another action"));
        let mut forged = Trigger::new(&k, "demo-bot", "nonce-2", 10_000);
        forged.time += 1;
        assert!(forged
            .verify(&k, "demo-bot", "sync", 10_000, &mut seen)
            .is_err());
        // a recent nonce is never forgotten while its trigger could still be accepted
        let mut seen = vec![];
        let t = Trigger::new(&k, "demo-bot", "keep", 10_000);
        t.verify(&k, "demo-bot", "sync", 10_000, &mut seen).unwrap();
        for i in 0..300 {
            Trigger::new(&k, "demo-bot", &format!("n{i}"), 10_000)
                .verify(&k, "demo-bot", "sync", 10_000, &mut seen)
                .unwrap();
        }
        assert!(t.verify(&k, "demo-bot", "sync", 10_200, &mut seen).is_err());
        // long after the window the old trigger is refused as too old, and the next accepted one prunes its nonce
        assert!(t.verify(&k, "demo-bot", "sync", 20_000, &mut seen).is_err());
        Trigger::new(&k, "demo-bot", "fresh", 20_000)
            .verify(&k, "demo-bot", "sync", 20_000, &mut seen)
            .unwrap();
        assert_eq!(seen, vec!["20000:fresh".to_string()]);
    }
}

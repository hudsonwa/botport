//! Loop/abuse guard: hop limit, daily caps with local-day rollover, and
//! one in-flight turn per direction. Pure logic; the clock is injected.

use crate::envelope::Kind;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub trait Clock {
    /// Current local calendar day.
    fn today(&self) -> NaiveDate;
}

pub struct LocalClock;
impl Clock for LocalClock {
    fn today(&self) -> NaiveDate {
        chrono::Local::now().date_naive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub hop_limit: u32,
    pub max_messages: u64,
    pub max_tokens: u64,
    pub max_wakes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            hop_limit: 3,
            max_messages: 200,
            max_tokens: 200_000,
            max_wakes: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    HopLimit { hop: u32, limit: u32 },
    NoReplyToKind(Kind),
    MessageCap,
    TokenCap,
    WakeCap,
    TurnInFlight(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub day: Option<NaiveDate>,
    pub messages: u64,
    pub tokens: u64,
    pub wakes: u64,
    #[serde(default)]
    pub ops_auto: u64,
    #[serde(default)]
    pub ops_approved: u64,
    #[serde(default)]
    pub ops_refused: u64,
}

/// How a Grok-requested op was decided, for the daily op counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpOutcome {
    Auto,
    Approved,
    Refused,
}

pub struct Guard<C: Clock> {
    pub limits: Limits,
    pub counters: Counters,
    in_flight: BTreeSet<String>,
    clock: C,
}

impl<C: Clock> Guard<C> {
    pub fn new(limits: Limits, clock: C) -> Self {
        Guard {
            limits,
            counters: Counters::default(),
            in_flight: BTreeSet::new(),
            clock,
        }
    }

    fn rollover(&mut self) {
        let today = self.clock.today();
        if self.counters.day != Some(today) {
            self.counters = Counters {
                day: Some(today),
                ..Counters::default()
            };
        }
    }

    /// May a message with this hop be delivered?
    pub fn check_hop(&self, hop: u32) -> Result<(), Refusal> {
        if hop >= self.limits.hop_limit {
            return Err(Refusal::HopLimit {
                hop,
                limit: self.limits.hop_limit,
            });
        }
        Ok(())
    }

    /// May we auto-reply to an inbound message of `kind` at `hop`?
    /// The reply would carry hop + 1, which must also be under the limit.
    pub fn may_auto_reply(&self, kind: Kind, hop: u32) -> Result<(), Refusal> {
        if matches!(kind, Kind::End | Kind::Ack | Kind::Notice) {
            return Err(Refusal::NoReplyToKind(kind));
        }
        self.check_hop(hop)?;
        self.check_hop(hop.saturating_add(1))
    }

    /// Account one message of `tokens` tokens; refuse (and count nothing) if a cap would be exceeded.
    pub fn record_message(&mut self, tokens: u64) -> Result<(), Refusal> {
        self.rollover();
        if self.counters.messages >= self.limits.max_messages {
            return Err(Refusal::MessageCap);
        }
        if self.counters.tokens.saturating_add(tokens) > self.limits.max_tokens {
            return Err(Refusal::TokenCap);
        }
        self.counters.messages += 1;
        self.counters.tokens += tokens;
        Ok(())
    }

    /// Is there room for `messages` more messages and `tokens` more tokens today?
    pub fn check_room(&mut self, messages: u64, tokens: u64) -> Result<(), Refusal> {
        self.rollover();
        if self.counters.messages.saturating_add(messages) > self.limits.max_messages {
            return Err(Refusal::MessageCap);
        }
        if self.counters.tokens.saturating_add(tokens) > self.limits.max_tokens {
            return Err(Refusal::TokenCap);
        }
        Ok(())
    }

    /// Today's counters (after any day rollover).
    pub fn today(&mut self) -> Counters {
        self.rollover();
        self.counters.clone()
    }

    pub fn record_wake(&mut self) -> Result<(), Refusal> {
        self.rollover();
        if self.counters.wakes >= self.limits.max_wakes {
            return Err(Refusal::WakeCap);
        }
        self.counters.wakes += 1;
        Ok(())
    }

    /// Count one decided op for today.
    pub fn record_op(&mut self, o: OpOutcome) {
        self.rollover();
        let c = &mut self.counters;
        match o {
            OpOutcome::Auto => c.ops_auto += 1,
            OpOutcome::Approved => c.ops_approved += 1,
            OpOutcome::Refused => c.ops_refused += 1,
        }
    }

    /// Start a turn for `direction` (e.g. "grok->hermes"); only one at a time.
    pub fn begin_turn(&mut self, direction: &str) -> Result<(), Refusal> {
        if !self.in_flight.insert(direction.to_string()) {
            return Err(Refusal::TurnInFlight(direction.to_string()));
        }
        Ok(())
    }

    pub fn end_turn(&mut self, direction: &str) {
        self.in_flight.remove(direction);
    }

    pub fn in_flight(&self, direction: &str) -> bool {
        self.in_flight.contains(direction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakeClock(Rc<Cell<NaiveDate>>);
    impl Clock for FakeClock {
        fn today(&self) -> NaiveDate {
            self.0.get()
        }
    }
    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }
    fn guard(l: Limits) -> (Guard<FakeClock>, Rc<Cell<NaiveDate>>) {
        let c = Rc::new(Cell::new(day(2)));
        (Guard::new(l, FakeClock(c.clone())), c)
    }

    #[test]
    fn hop_three_refused() {
        let (g, _) = guard(Limits::default());
        assert!(g.check_hop(2).is_ok());
        assert_eq!(g.check_hop(3), Err(Refusal::HopLimit { hop: 3, limit: 3 }));
        assert!(g.may_auto_reply(Kind::Msg, 1).is_ok());
        assert!(g.may_auto_reply(Kind::Msg, 2).is_err());
    }

    #[test]
    fn no_reply_to_end_or_ack() {
        let (g, _) = guard(Limits::default());
        assert_eq!(
            g.may_auto_reply(Kind::End, 0),
            Err(Refusal::NoReplyToKind(Kind::End))
        );
        assert_eq!(
            g.may_auto_reply(Kind::Ack, 0),
            Err(Refusal::NoReplyToKind(Kind::Ack))
        );
    }

    #[test]
    fn caps_exhaust_and_roll_over() {
        let (mut g, c) = guard(Limits {
            max_messages: 2,
            max_tokens: 100,
            max_wakes: 1,
            ..Limits::default()
        });
        assert!(g.record_message(10).is_ok());
        assert!(g.record_message(10).is_ok());
        assert_eq!(g.record_message(1), Err(Refusal::MessageCap));
        assert!(g.record_wake().is_ok());
        assert_eq!(g.record_wake(), Err(Refusal::WakeCap));
        c.set(day(3));
        assert!(g.record_message(100).is_ok());
        assert_eq!(g.record_message(1), Err(Refusal::TokenCap));
        assert!(g.record_wake().is_ok());
        assert_eq!(g.counters.messages, 1);
    }

    #[test]
    fn ops_counted_per_day() {
        let (mut g, c) = guard(Limits::default());
        g.record_op(OpOutcome::Auto);
        g.record_op(OpOutcome::Auto);
        g.record_op(OpOutcome::Refused);
        let t = g.today();
        assert_eq!((t.ops_auto, t.ops_approved, t.ops_refused), (2, 0, 1));
        c.set(day(3));
        g.record_op(OpOutcome::Approved);
        let t = g.today();
        assert_eq!((t.ops_auto, t.ops_approved, t.ops_refused), (0, 1, 0));
    }

    #[test]
    fn old_counters_load_without_op_fields() {
        let c: Counters =
            serde_json::from_str(r#"{"day":null,"messages":1,"tokens":2,"wakes":3}"#).unwrap();
        assert_eq!((c.wakes, c.ops_auto), (3, 0));
    }

    #[test]
    fn defaults() {
        let l = Limits::default();
        assert_eq!(
            (l.hop_limit, l.max_messages, l.max_tokens, l.max_wakes),
            (3, 200, 200_000, 10)
        );
    }

    #[test]
    fn one_turn_per_direction() {
        let (mut g, _) = guard(Limits::default());
        assert!(g.begin_turn("a->b").is_ok());
        assert!(g.begin_turn("b->a").is_ok());
        assert!(g.begin_turn("a->b").is_err());
        g.end_turn("a->b");
        assert!(!g.in_flight("a->b"));
        assert!(g.begin_turn("a->b").is_ok());
    }

    proptest! {
        #[test]
        fn never_exceeds_daily_caps(
            cap in 0u64..20, tok_cap in 0u64..500,
            ops in prop::collection::vec((0u64..60, any::<bool>(), 0u8..10), 0..200)
        ) {
            let (mut g, c) = guard(Limits { max_messages: cap, max_tokens: tok_cap, max_wakes: cap, ..Limits::default() });
            let mut d = 2;
            for (tokens, wake, advance) in ops {
                if advance == 0 { d = (d % 27) + 1; c.set(day(d)); }
                if wake { let _ = g.record_wake(); } else { let _ = g.record_message(tokens); }
                prop_assert!(g.counters.messages <= cap);
                prop_assert!(g.counters.wakes <= cap);
                prop_assert!(g.counters.tokens <= tok_cap);
            }
        }

        #[test]
        fn hop_never_exceeds_limit(limit in 0u32..10, hop in 0u32..20, end in any::<bool>()) {
            let (g, _) = guard(Limits { hop_limit: limit, ..Limits::default() });
            if g.check_hop(hop).is_ok() { prop_assert!(hop < limit); }
            let kind = if end { Kind::End } else { Kind::Msg };
            if g.may_auto_reply(kind, hop).is_ok() { prop_assert!(hop + 1 < limit && !end); }
        }
    }

    #[test]
    fn auto_reply_at_max_hop_is_refused_not_overflow() {
        let (g, _) = guard(Limits {
            hop_limit: u32::MAX,
            ..Limits::default()
        });
        assert!(g.may_auto_reply(Kind::Msg, u32::MAX).is_err());
    }
}

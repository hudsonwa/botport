//! Low findings from the rc/0.3.4 review: ledger write errors surface, audit matches whole words.

use botport_grok::log::TurnLedger;
use botport_grok::routines::contains_word;

#[test]
fn ledger_write_error_is_returned_not_swallowed() {
    let d = std::env::temp_dir().join(format!("botport-r034-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let blocker = d.join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let bad = TurnLedger::new(Some(blocker.join("grok-turns.jsonl")));
    assert!(bad.record(1, "send", "cli", "b1").is_err());
    let good = TurnLedger::new(Some(d.join("grok-turns.jsonl")));
    good.record(1, "send", "cli", "b1").unwrap();
    assert_eq!(good.this_week(2), 1);
    assert!(TurnLedger::off().record(1, "send", "cli", "b1").is_ok());
    let _ = std::fs::remove_file(d.join("grok-turns.jsonl"));
    let _ = std::fs::remove_file(&blocker);
    let _ = std::fs::remove_dir(&d);
}

#[test]
fn audit_used_name_matches_whole_words_only() {
    let hay = "demo reminder daily-digest mirror alpha-beta";
    assert!(!contains_word(hay, "a"));
    assert!(!contains_word(hay, "demo rem"));
    assert!(contains_word(hay, "alpha"));
    assert!(!contains_word(hay, "alph"));
    assert!(!contains_word(hay, "mirro"));
    assert!(contains_word(hay, "demo reminder"));
    assert!(contains_word(hay, "daily-digest"));
    assert!(contains_word(hay, "alpha-beta"));
    assert!(contains_word("x (mirror) y", "mirror"));
    assert!(!contains_word(hay, ""));
}

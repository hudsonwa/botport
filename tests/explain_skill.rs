//! botport#67: the botport-explain skill is shipped, Hermes-format, read-only, and free of hosts and addresses.

const SKILL: &str = include_str!("../docs/skills/botport-explain/SKILL.md");

#[test]
fn explain_skill_is_hermes_format_and_tagged_plugin() {
    assert!(SKILL.starts_with("---\nname: botport-explain\n"), "{SKILL}");
    let front = SKILL.split("---").nth(1).expect("front matter");
    assert!(front.contains("description: "));
    assert!(front.contains("origin: plugin"));
    for word in [
        "**export**",
        "**import**",
        "**mirror**",
        "read-only",
        "botport --version",
        "SHA256SUMS",
    ] {
        assert!(SKILL.contains(word), "missing {word}");
    }
}

#[test]
fn explain_skill_has_no_hosts_addresses_or_home_paths() {
    for bad in [
        "/Users/",
        "/home/",
        ".local",
        ".lan",
        "ts.net",
        "tailscale",
        "localhost",
        "127.0.0.1",
    ] {
        assert!(
            !SKILL.to_lowercase().contains(&bad.to_lowercase()),
            "found {bad}"
        );
    }
    // no dotted IPv4 address
    for word in SKILL.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = word.split('.').filter(|p| !p.is_empty()).collect();
        assert!(
            !(parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())),
            "found address {word}"
        );
    }
    assert!(SKILL.contains("(invented)"));
}

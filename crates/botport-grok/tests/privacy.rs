//! The committed fixtures are synthetic: no real ids, home paths, hosts, keys or e-mail addresses.
//! (The full gate, with the private denylist and raw values, runs before anything is pushed.)
mod common;

fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().is_dir() {
            files(&e.path(), out);
        } else {
            out.push(e.path());
        }
    }
}

#[test]
fn fixtures_are_synthetic() {
    let mut fs = vec![];
    files(&common::fx(), &mut fs);
    assert_eq!(fs.len(), 17);
    let uuid = |w: &str| {
        w.len() == 36
            && w.chars().enumerate().all(|(i, c)| {
                if [8, 13, 18, 23].contains(&i) {
                    c == '-'
                } else {
                    c.is_ascii_hexdigit()
                }
            })
    };
    for f in fs {
        let s = std::fs::read_to_string(&f).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["synthetic"], true, "{}", f.display());
        for bad in [
            "crsr_",
            "/Users/",
            &["automations", "webhook", ""].join("/"),
            ".ts.net",
            ".lan\"",
            "Bearer ",
            "@gmail",
            "@icloud",
        ] {
            assert!(!s.contains(bad), "{} contains {bad:?}", f.display());
        }
        for w in s.split(|c: char| !(c.is_ascii_hexdigit() || c == '-')) {
            if uuid(w) {
                assert!(
                    w.starts_with("00000000-0000-4000-8000-"),
                    "{}: non-synthetic uuid",
                    f.display()
                );
            }
        }
        for (i, _) in s.match_indices("/home/") {
            assert!(
                s[i..].starts_with("/home/box/"),
                "{}: home path",
                f.display()
            );
        }
    }
}

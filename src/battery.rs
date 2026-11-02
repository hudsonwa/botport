//! `test`: a scripted CLI battery through `hermes chat -p <bot> --format stream-json` (tool calls visible)
//! plus an optional gateway turn through `hermes -p <bot> -z`; `battery init` writes a starter battery from a bundle.
//! Nothing here assumes a particular bot: every expectation comes from the battery file.
use crate::core::adapter::Source;
use crate::core::model::BotModel;
use crate::core::util::*;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
pub struct Battery {
    #[serde(default)]
    pub gateway: Option<Gateway>,
    pub cases: Vec<Case>,
    #[serde(default = "gap")]
    pub gap_s: u64,
}
fn gap() -> u64 {
    3
}

#[derive(Deserialize)]
pub struct Gateway {
    pub prompt: String,
    pub expect: String,
}

#[derive(Deserialize, Clone)]
pub struct Case {
    pub id: String,
    pub family: String,
    pub prompt: String,
    /// any-of: a tool name or prefix that must be called ([] = none needed; "*" = any tool)
    #[serde(default)]
    pub want: Vec<String>,
    #[serde(default)]
    pub forbid: Vec<String>,
    /// all must match the answer, case-insensitive
    #[serde(default)]
    pub patterns: Vec<String>,
    /// none may match the answer, case-insensitive
    #[serde(default)]
    pub absent: Vec<String>,
    // `generated` and `why` (written by `battery init`) are notes for the owner; serde ignores them here.
}

pub fn load(p: &Path) -> Res<Battery> {
    serde_json::from_slice(&read(p)?).map_err(|e| format!("{}: {e}", p.display()))
}

/// Score one stream-json transcript (pure: tested without hermes).
pub fn score(case: &Case, stdout: &str, rc: i32) -> Value {
    let mut tools: Vec<String> = vec![];
    let mut text = String::new();
    let mut result_text = String::new();
    for line in stdout.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match e["type"].as_str() {
            Some("tool_use") => tools.push(e["name"].as_str().unwrap_or("").to_string()),
            Some("text") => text += e["text"].as_str().unwrap_or(""),
            Some("result") => result_text = e["text"].as_str().unwrap_or("").to_string(),
            _ => {}
        }
    }
    // A turn stopped by the tool-turn limit streams no text: its forced summary is only in the `result` record.
    let answer = if text.trim().is_empty() {
        result_text.trim().to_string()
    } else {
        text.trim().to_string()
    };
    let mut why = vec![];
    let hit = |t: &str, w: &str| w == "*" || t == w || t.starts_with(w);
    if !case.want.is_empty() && !case.want.iter().any(|w| tools.iter().any(|t| hit(t, w))) {
        why.push(format!("expected one of {:?}", case.want));
    }
    let bad: Vec<&String> = tools
        .iter()
        .filter(|t| case.forbid.iter().any(|f| hit(t, f)))
        .collect();
    if !bad.is_empty() {
        why.push(format!("forbidden tool {bad:?}"));
    }
    for p in &case.patterns {
        match RegexBuilder::new(p).case_insensitive(true).build() {
            Ok(r) if r.is_match(&answer) => {}
            Ok(_) => why.push(format!("answer lacks /{p}/")),
            Err(e) => why.push(format!("bad pattern /{p}/: {e}")),
        }
    }
    for p in &case.absent {
        match RegexBuilder::new(p).case_insensitive(true).build() {
            Ok(r) if r.is_match(&answer) => why.push(format!("answer has /{p}/")),
            Ok(_) => {}
            Err(e) => why.push(format!("bad pattern /{p}/: {e}")),
        }
    }
    if rc != 0 {
        why.push(
            if text.trim().is_empty() && !result_text.trim().is_empty() {
                format!(
                    "rc={rc} (hit the tool-turn limit after {} tool calls)",
                    tools.len()
                )
            } else {
                format!("rc={rc}")
            },
        );
    }
    let a: String = answer.chars().take(1500).collect();
    json!({"id": case.id, "family": case.family, "pass": why.is_empty(), "why": why, "tools": tools, "answer": a})
}

pub fn run(
    hermes: &str,
    profile: &str,
    b: &Battery,
    only: &[String],
    workdir: &Path,
) -> Res<Vec<Value>> {
    let mut res = vec![];
    let want = |id: &str| only.is_empty() || only.iter().any(|o| o == id);
    if let Some(g) = &b.gateway {
        if want("gateway-oneshot") {
            let t0 = Instant::now();
            let o = Command::new(hermes)
                .args(["-p", profile, "-z", &g.prompt])
                .output()
                .map_err(|e| format!("{hermes}: {e}"))?;
            let out = String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_end_matches('.')
                .to_lowercase();
            let ok = o.status.success() && out == g.expect.to_lowercase();
            res.push(json!({"id": "gateway-oneshot", "family": "gateway", "pass": ok,
                            "why": if ok { vec![] } else { vec![format!("rc={:?} out={:?}", o.status.code(), out.chars().take(80).collect::<String>())] },
                            "tools": [], "wall_s": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0}));
        }
    }
    for c in b.cases.iter().filter(|c| want(&c.id)) {
        std::thread::sleep(Duration::from_secs(b.gap_s));
        let t0 = Instant::now();
        let o = Command::new(hermes)
            .args([
                "chat",
                "-p",
                profile,
                "-q",
                &c.prompt,
                "--format",
                "stream-json",
                "--source",
                "tool",
                "--in",
            ])
            .arg(workdir)
            .output()
            .map_err(|e| format!("{hermes}: {e}"))?;
        let mut r = score(
            c,
            &String::from_utf8_lossy(&o.stdout),
            o.status.code().unwrap_or(-1),
        );
        r["wall_s"] = json!((t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        eprintln!(
            "{} {} {}s",
            if r["pass"] == json!(true) {
                "PASS"
            } else {
                "FAIL"
            },
            c.id,
            r["wall_s"]
        );
        res.push(r);
    }
    Ok(res)
}

/// A deterministic starter battery: persona, one case per skill, per schedule/connector named in NOT_CAPTURED.md,
/// a few memory facts, and live-status questions that must use a tool. Output depends only on the bundle content.
pub fn init(m: &BotModel, src: &Source, not_captured: &str) -> Value {
    let mut cases = vec![];
    let mut add = |id: String,
                   family: &str,
                   prompt: String,
                   want: Vec<&str>,
                   patterns: Vec<String>,
                   why: &str| {
        cases.push(
            json!({"id": id, "family": family, "prompt": prompt, "want": want, "forbid": [],
                          "patterns": patterns, "absent": [], "generated": true, "why": why}),
        );
    };
    let first = m.persona.name.split_whitespace().next().unwrap_or("");
    add(
        "persona".into(),
        "persona",
        "Who are you? One sentence.".into(),
        vec![],
        if first.is_empty() {
            vec![]
        } else {
            vec![regex::escape(first)]
        },
        "the bot should introduce itself by its profile name",
    );
    let mut skills: Vec<_> = m.skills.iter().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    for s in skills {
        let dir = &s.name;
        let md = s.files.get("SKILL.md").cloned().unwrap_or_default();
        let title = front(&md, "name").unwrap_or_else(|| dir.clone());
        add(
            format!("skill-{}", crate::adapters::hermes::spec::slug(dir)),
            "skill",
            format!("What does your \"{title}\" skill tell you to do? Two sentences."),
            vec!["skill_view", "skills_list"],
            vec![],
            "an imported skill should be found and read, not guessed",
        );
    }
    for (family, item) in described_items(not_captured) {
        let label = item
            .split([':', '—', '('])
            .next()
            .unwrap_or(&item)
            .trim()
            .to_string();
        let prompt = if family == "schedule" {
            format!("Do you have a scheduled job for \"{label}\"? When does it run?")
        } else {
            format!("Can you use the \"{label}\" connector right now? Check, then answer in one sentence.")
        };
        add(
            format!("{family}-{}", crate::adapters::hermes::spec::slug(&label)),
            family,
            prompt,
            vec!["*"],
            vec![],
            "listed in NOT_CAPTURED.md: confirm it was re-created on the Hermes side",
        );
    }
    for (i, (prompt, pat)) in memory_cases(m, src).into_iter().enumerate() {
        add(
            format!("memory-{}", i + 1),
            "memory",
            prompt,
            vec![],
            vec![regex::escape(&pat)],
            "a fact from the bot's memory",
        );
    }
    add(
        "live-time".into(),
        "live",
        "What is the date and time right now? Check with a tool, don't guess.".into(),
        vec!["*"],
        vec![r"\d".into()],
        "live status must come from a tool call, not memory",
    );
    add(
        "live-skills".into(),
        "live",
        "Which skills can you use right now? List their names.".into(),
        vec!["skills_list", "skill_view"],
        vec![],
        "the skill list must be read live",
    );
    json!({"gap_s": 3, "cases": cases})
}

fn front(md: &str, key: &str) -> Option<String> {
    let body = md.strip_prefix("---\n")?;
    let fm = &body[..body.find("\n---")?];
    fm.lines()
        .find_map(|l| {
            l.strip_prefix(&format!("{key}:"))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .filter(|v| !v.is_empty())
}

/// Schedule and connector names from the structured description; bullets under matching headings when the
/// description parser finds none (free-form text).
fn described_items(md: &str) -> Vec<(&'static str, String)> {
    let d = crate::adapters::grok_bot::describe::parse(md);
    let names = |k: &str| -> Vec<String> {
        d[k].as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["name"].as_str().map(String::from))
            .collect()
    };
    let mut out: Vec<(&'static str, String)> = names("schedules")
        .into_iter()
        .map(|n| ("schedule", n))
        .chain(names("connectors").into_iter().map(|n| ("connector", n)))
        .collect();
    if out.is_empty() {
        out = not_captured_items(md);
    }
    out
}

/// Bullets under headings that mention schedules or connectors (fallback for free-form descriptions).
fn not_captured_items(md: &str) -> Vec<(&'static str, String)> {
    let mut fam = None;
    let mut out = vec![];
    for l in md.lines() {
        let t = l.trim();
        if t.starts_with('#') {
            let h = t.to_lowercase();
            fam = if ["schedule", "automation", "watcher", "cron"]
                .iter()
                .any(|k| h.contains(k))
            {
                Some("schedule")
            } else if ["connector", "integration", "mcp"]
                .iter()
                .any(|k| h.contains(k))
            {
                Some("connector")
            } else {
                None
            };
        } else if let (Some(f), Some(item)) =
            (fam, t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")))
        {
            let item = item.trim().trim_matches('*').trim();
            if !item.is_empty() {
                out.push((f, item.to_string()));
            }
        }
    }
    out
}

/// Up to 3 facts (memory.jsonl rows first, else MEMORY.md bullets): ask for the first half, expect a word from the rest.
fn memory_cases(m: &BotModel, src: &Source) -> Vec<(String, String)> {
    let p = src.path.join("memory/memory.jsonl");
    let mut facts: Vec<String> = if p.is_file() {
        crate::adapters::grok_bot::bundle::parse_jsonl(&String::from_utf8_lossy(
            &read(&p).unwrap_or_default(),
        ))
        .iter()
        .filter_map(|r| r["fact"].as_str().map(String::from))
        .collect()
    } else {
        vec![]
    };
    let bullets = |lines: Vec<&str>| -> Vec<String> {
        lines
            .into_iter()
            .filter_map(|l| l.trim().strip_prefix("- ").map(String::from))
            .collect()
    };
    if facts.is_empty() {
        // the bundle's MEMORY.md lines, or each fact of a grok-bot-export file as written
        facts = match &m.memory_text {
            Some(t) => bullets(t.lines().collect()),
            None => bullets(m.memory.iter().map(|f| f.text.as_str()).collect()),
        };
    }
    // a grok-bot-export file keeps its memories as MEMORY.md bullets, its facts unprefixed
    if facts.is_empty() && m.memory_text.is_none() {
        let md: String = m
            .memory
            .iter()
            .filter(|f| f.scope.as_deref() != Some("user"))
            .map(|f| format!("- {}\n", f.text))
            .collect();
        facts = bullets(md.lines().collect());
    }
    let mut out = vec![];
    for f in facts {
        let words: Vec<&str> = f.split_whitespace().collect();
        if words.len() < 4 {
            continue;
        }
        let (head, tail) = words.split_at(words.len() / 2);
        let clean = |w: &&str| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
        let key = tail
            .iter()
            .map(clean)
            .find(|w| w.chars().any(|c| c.is_ascii_digit()))
            .or_else(|| {
                tail.iter()
                    .map(clean)
                    .filter(|w| w.len() > 3)
                    .max_by_key(|w| w.len())
            });
        if let Some(k) = key.filter(|k| !k.is_empty()) {
            out.push((
                format!("From your memory, complete this: \"{} …\"", head.join(" ")),
                k,
            ));
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case() -> Case {
        Case {
            id: "c".into(),
            family: "f".into(),
            prompt: "p".into(),
            want: vec!["mcp__knowledge__".into()],
            forbid: vec!["terminal".into()],
            patterns: vec![r"\bDemo\b".into()],
            absent: vec!["secret".into()],
        }
    }

    #[test]
    fn passes_with_tool_and_pattern() {
        let s = "{\"type\":\"tool_use\",\"name\":\"mcp__knowledge__health\"}\n{\"type\":\"text\",\"text\":\"I am demo\"}\n";
        assert_eq!(score(&case(), s, 0)["pass"], json!(true));
    }

    #[test]
    fn absent_pattern_fails_the_case() {
        let s = "{\"type\":\"tool_use\",\"name\":\"mcp__knowledge__health\"}\n{\"type\":\"text\",\"text\":\"Demo secret\"}\n";
        assert_eq!(score(&case(), s, 0)["pass"], json!(false));
    }

    #[test]
    fn fails_on_forbidden_missing_and_rc() {
        let s = "{\"type\":\"tool_use\",\"name\":\"terminal\"}\nnot json\n{\"type\":\"text\",\"text\":\"nope\"}\n";
        let r = score(&case(), s, 1);
        assert_eq!(r["pass"], json!(false));
        assert_eq!(r["why"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn turn_limit_summary_is_scored_but_still_fails() {
        let s = "{\"type\":\"tool_use\",\"name\":\"mcp__knowledge__health\"}\n\
                 {\"type\":\"result\",\"exit_code\":1,\"text\":\"Demo says hi\"}\n";
        let r = score(&case(), s, 1);
        assert_eq!(r["answer"], json!("Demo says hi"));
        assert_eq!(r["pass"], json!(false));
        assert_eq!(
            r["why"],
            json!(["rc=1 (hit the tool-turn limit after 1 tool calls)"])
        );
    }

    #[test]
    fn streamed_text_wins_over_result_text() {
        let s = "{\"type\":\"tool_use\",\"name\":\"mcp__knowledge__health\"}\n{\"type\":\"text\",\"text\":\"I am Demo\"}\n\
                 {\"type\":\"result\",\"exit_code\":0,\"text\":\"other\"}\n";
        let r = score(&case(), s, 0);
        assert_eq!(r["answer"], json!("I am Demo"));
        assert_eq!(r["pass"], json!(true));
    }
}

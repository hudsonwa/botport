// botport (formerly grok2hermes) — export a Grok Bot and import/sync it into a Hermes profile. Deterministic; terse by default:
// every command prints at most 10 summary lines (or one JSON document with --json) and writes full detail to a file.
// The CLI on top of the library (src/lib.rs): argument parsing, reports, and the seam wired end to end.
use botport::adapters::grok_bot::{bundle, describe, export_json, GrokBot};
use botport::adapters::hermes::{spec, sync, twoway, HermesProfile};
use botport::core::adapter::{retain_skills, Adapter, Item, Source, St, Target};
use botport::core::util;
use botport::{adapters, battery, doctor};

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;
use sync::Ctx;
use util::*;

const USAGE: &str = "botport <command> [options]   (v{V}; --version prints it)
  export  --out DIR [--from-dir DIR | --agent-data DIR] [--agent-id ID] [--memory-drop DIR]
          [--history-db FILE | --history-sqlite FILE] [--describe FILE] [--max-memory-age-days 2] [--strict]
  verify  --bundle DIR
  describe --bundle DIR [--describe FILE] [--out FILE|-] [--json]   NOT_CAPTURED.md -> describe.json (default ./describe.json; - = stdout)
  plan    --bundle DIR [--spec FILE] [--profile NAME] [--no-cron]   dry run: what apply would do
  apply   --bundle DIR [--spec FILE] [--profile NAME] [--adopt] [--dry-run] [--no-cron]
  sync    --bundle DIR [--spec FILE] [--profile NAME] [--adopt] [--no-cron]   apply only if the export or profile changed
  drift   --bundle DIR [--spec FILE] [--profile NAME] [--no-cron]   exit 1 on any difference (both directions)
  export-hermes --bundle GROKDIR [--spec FILE] [--profile NAME] [--out DIR] [--dry-run]
          what the Hermes bot learned (facts, skills, cron) as a bundle for the Grok bot (docs/AGENT-IMPORT.md)
  (sync/drift: --direction both|g2h|h2g; default both when the spec has [hermes_export], else g2h)
  test    --profile NAME --battery FILE [--only id,id] [--out DIR]
  doctor  [--profile NAME | --profile-dir DIR] [--spec FILE]   check hermes (on PATH, version), profile dir, write access, spec; exit 1 on a failure
  battery init PATH|--bundle PATH [--spec FILE] --out FILE  starter battery (PATH: bundle dir or grok-bot-export file) (docs/USER-GUIDE.md §6)
common: --hermes-home DIR (default $HERMES_HOME, else ~/.hermes) | --profile-dir DIR  --hermes-bin PATH  --json  --details FILE";

const VALUED: &[&str] = &[
    "out",
    "from-dir",
    "agent-data",
    "agent-id",
    "memory-drop",
    "history-db",
    "history-sqlite",
    "describe",
    "max-memory-age-days",
    "direction",
    "bundle",
    "spec",
    "profile",
    "profile-dir",
    "hermes-home",
    "hermes-bin",
    "details",
    "battery",
    "only",
];
const FLAGS: &[&str] = &[
    "strict", "json", "adopt", "dry-run", "help", "no-cron", "version",
];

struct Report {
    ok: bool,
    code: u8,
    lines: Vec<String>,
    json: Value,
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args()
        .skip(1)
        .map(|s| match s.as_str() {
            "-V" => "--version".into(),
            "-h" => "--help".into(),
            _ => s,
        })
        .collect();
    let a = match Args::parse(&raw, VALUED, FLAGS) {
        Ok(a) => a,
        Err(e) => return fail(&e),
    };
    let cmd = a.pos.first().cloned().unwrap_or_default();
    if a.has("version") {
        println!("botport {}", bundle::VERSION);
        return ExitCode::SUCCESS;
    }
    if cmd.is_empty() && !a.has("help") {
        eprintln!("{}", USAGE.replace("{V}", bundle::VERSION));
        return ExitCode::from(2);
    }
    if a.has("help") || cmd == "help" {
        println!("{}", USAGE.replace("{V}", bundle::VERSION));
        return ExitCode::SUCCESS;
    }
    let r = match cmd.as_str() {
        "export" => cmd_export(&a),
        "verify" => cmd_verify(&a),
        "describe" => cmd_describe(&a),
        "plan" | "apply" | "sync" | "drift" | "export-hermes" => cmd_profile(&a, &cmd),
        "test" => cmd_test(&a),
        "doctor" => cmd_doctor(&a),
        "battery" if a.pos.get(1).map(String::as_str) == Some("init") => cmd_battery_init(&a),
        _ => Err(format!("unknown command {cmd:?} (try --help)")),
    };
    match r {
        Ok(r) => emit(&a, &cmd, r),
        Err(e) => fail(&e),
    }
}

fn fail(e: &str) -> ExitCode {
    eprintln!("botport: ERROR: {e}");
    ExitCode::from(2)
}

/// Full detail to a file; stdout gets <=10 lines (or the JSON).
fn emit(a: &Args, cmd: &str, r: Report) -> ExitCode {
    let name = match r.json["profile"].as_str().or(a.get("profile")) {
        Some(p) => format!("{cmd}-{p}.json"),
        None => format!("{cmd}.json"),
    };
    // Per-user state dir (not a shared /tmp name another user or run could own).
    let details = a.path("details").unwrap_or_else(|| {
        let env = |k| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let dir = env("XDG_STATE_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".local/state")))
            .unwrap_or_else(std::env::temp_dir);
        dir.join("botport").join(name)
    });
    // stdout is an output too: the same guard as every file write
    if let Err(e) = botport::core::secrets::guard("the report", json_sorted(&r.json).as_bytes())
        .and_then(|_| botport::core::secrets::guard("the summary", r.lines.join("\n").as_bytes()))
    {
        eprintln!("botport: ERROR: {e}");
        return ExitCode::from(2);
    }
    let wrote = write_atomic(&details, json_sorted(&r.json).as_bytes(), Some(0o600)).is_ok();
    let quiet_ok = r.json["in_sync"] == json!(true); // nothing to look up: skip the details line
                                                     // describe --out - puts describe.json on stdout, so the report goes to stderr
    let to_stderr = cmd == "describe" && a.get("out") == Some("-");
    if a.has("json") {
        if to_stderr {
            eprint!("{}", json_sorted(&r.json));
        } else {
            print!("{}", json_sorted(&r.json));
        }
    } else {
        let mut lines = r.lines;
        if wrote && !quiet_ok {
            lines.push(format!("details: {}", details.display()));
        }
        if lines.len() > 10 {
            let extra = lines.len() - 9;
            let last = lines.pop().unwrap();
            lines.truncate(8);
            lines.push(format!("  … {} more (see details)", extra));
            lines.push(last);
        }
        if to_stderr {
            eprintln!("{}", lines.join("\n"));
        } else {
            println!("{}", lines.join("\n"));
        }
    }
    let _ = r.ok;
    ExitCode::from(r.code)
}

fn cmd_export(a: &Args) -> Res<Report> {
    let o = bundle::ExportOpts {
        out: a.path("out").ok_or("--out is required")?,
        from_dir: a.path("from-dir"),
        agent_data: a
            .path("agent-data")
            .or_else(|| std::env::var_os("BOTPORT_AGENT_DATA").map(PathBuf::from))
            // the Grok Bot sandbox layout (export runs inside the Grok box)
            .unwrap_or_else(|| PathBuf::from("/home/box/agent-data")),
        agent_id: a.get("agent-id").map(String::from),
        memory_drop: a
            .path("memory-drop")
            .or_else(|| std::env::var_os("BOTPORT_MEMORY_DROP").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/workspace/exports/memory-drop")),
        history_db: a.path("history-db"),
        history_sqlite: a.path("history-sqlite"),
        describe: a.path("describe"),
        max_memory_age_days: a
            .get("max-memory-age-days")
            .map(|s| s.parse().map_err(|_| "bad --max-memory-age-days"))
            .transpose()?
            .unwrap_or(2.0),
        strict: a.has("strict"),
    };
    let r = bundle::export(&o)?;
    let digest = digest_pairs(&r.files);
    let mut lines = vec![format!(
        "export OK: {} files, {} skills, memory {}, history {}, manifest {}",
        r.files.len(),
        r.skills,
        r.memory_rows
            .map(|n| format!("{n} rows"))
            .unwrap_or("MEMORY.md/none".into()),
        r.history,
        short(&digest)
    )];
    if !r.excluded.is_empty() {
        lines.push(format!("left out as secrets: {}", r.excluded.len()));
    }
    for w in &r.warnings {
        lines.push(format!("WARN {w}"));
    }
    lines.push(format!("bundle: {}", o.out.display()));
    Ok(Report {
        ok: true,
        code: 0,
        lines,
        json: json!({"command": "export", "files": r.files, "digest": digest,
        "skills": r.skills, "memory_rows": r.memory_rows, "history": r.history, "excluded": r.excluded,
        "warnings": r.warnings, "bundle": o.out}),
    })
}

fn cmd_describe(a: &Args) -> Res<Report> {
    let src = match (a.path("describe"), a.path("bundle")) {
        (Some(f), _) => f,
        (None, Some(d)) => d.join("NOT_CAPTURED.md"),
        (None, None) => return err("--bundle or --describe is required"),
    };
    let v = describe::parse(&read_str(&src)?);
    let out = a
        .path("out")
        .unwrap_or_else(|| PathBuf::from("describe.json"));
    if out.as_os_str() == "-" {
        botport::core::secrets::guard("describe.json", json_sorted(&v).as_bytes())?;
        print!("{}", json_sorted(&v));
    } else {
        write_atomic(&out, json_sorted(&v).as_bytes(), Some(0o644))?;
    }
    let n = |k: &str| v[k].as_array().map_or(0, |x| x.len());
    let other = v["schedules"]
        .as_array()
        .map_or(0, |x| x.iter().filter(|s| s["side"] == "other").count());
    let mut lines = vec![format!("describe: {} instructions, {} schedules ({} on another machine), {} connectors, {} credentials, {} unparsed",
        n("instructions"), n("schedules"), other, n("connectors"), n("credentials"), n("unparsed"))];
    lines.extend(
        v["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| w.as_str())
            .map(|w| format!("WARN {w}")),
    );
    if n("unparsed") > 0 {
        lines.push(format!(
            "unparsed lines need a human: see {}",
            out.display()
        ));
    }
    lines.push(format!("wrote: {}", out.display()));
    Ok(Report {
        ok: true,
        code: 0,
        lines,
        json: json!({"command": "describe", "source": src, "out": out, "describe": v}),
    })
}

fn cmd_verify(a: &Args) -> Res<Report> {
    let dir = a.path("bundle").ok_or("--bundle is required")?;
    if export_json::is_export(&dir) {
        return verify_export(&dir);
    }
    let v = bundle::verify(&dir)?;
    let mut lines = vec![format!(
        "verify {}: {} files, manifest {}",
        if v.ok { "OK" } else { "FAILED" },
        v.files,
        short(&v.digest)
    )];
    lines.extend(v.problems.iter().map(|p| format!("  {p}")));
    Ok(Report {
        ok: v.ok,
        code: if v.ok { 0 } else { 1 },
        lines,
        json: json!({"command": "verify", "ok": v.ok, "files": v.files, "digest": v.digest, "problems": v.problems}),
    })
}

/// `verify` on a grok-bot-export.json: it parses, is v1, carries no secret-looking keys (no manifest to check).
fn verify_export(p: &std::path::Path) -> Res<Report> {
    let m = adapters::grok_bot::GrokBot.read(p)?;
    let truncated = export_json::truncated_count(p)?;
    let mut lines = vec![format!(
        "verify OK: {} export {:?}: {} facts, {} skills, {} routines, {} MCP apps",
        export_json::FORMAT,
        m.persona.name,
        m.memory.len(),
        m.skills.len(),
        m.schedules.len(),
        m.connectors.len()
    )];
    if truncated > 0 {
        lines.push(format!(
            "WARN {truncated} fact(s) marked truncated by the bot (kept as written)"
        ));
    }
    Ok(Report {
        ok: true,
        code: 0,
        lines,
        json: json!({"command": "verify", "ok": true, "format": export_json::FORMAT, "name": m.persona.name,
        "facts": m.memory.len(), "skills": m.skills.len(), "truncated_facts": truncated}),
    })
}

fn cmd_profile(a: &Args, cmd: &str) -> Res<Report> {
    let bdir = a.path("bundle").ok_or("--bundle is required")?;
    // The seam: the source adapter reads a neutral model; the Hermes target plans and applies it.
    let (mut m, mut src) = GrokBot.read_source(&bdir)?;
    let sp = match a.path("spec") {
        Some(p) => spec::Spec::load(
            &p,
            &a.path("hermes-home").unwrap_or_else(default_hermes_home),
        )?,
        None => spec::Spec::default(),
    };
    let hh = a.path("hermes-home").unwrap_or_else(default_hermes_home);
    let (profile, dest, real_home) = match a.path("profile-dir") {
        // A profile directory given directly (the agent-importers shim passes HERMES_HOME): cron is skipped.
        Some(d) => (
            d.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or("default".into()),
            d,
            false,
        ),
        None => {
            let p = a
                .get("profile")
                .map(String::from)
                .or(sp.profile.clone())
                .ok_or("--profile is required (or `profile` in the spec, or --profile-dir)")?;
            let d = if p == "default" {
                hh.clone()
            } else {
                botport::core::util::plain_name("profile name", &p)?;
                hh.join("profiles").join(&p)
            };
            // an explicit --hermes-bin (e.g. a test double) also manages cron in a non-default home
            (
                p,
                d,
                (hh == home().join(".hermes") || a.get("hermes-bin").is_some())
                    && !a.has("no-cron"),
            )
        }
    };
    // Allowlist: drop the Grok skills this profile doesn't take. Skills that originated on the Hermes side (sent by
    // h2g and now back in the Grok export) are always kept, so the two-way rules keep seeing them.
    let sent = sync::read_lock(&dest)
        .map(|l| l.h2g_skills)
        .unwrap_or_default();
    retain_skills(&mut m, &mut src, |n| {
        sp.skill_allowed(n) || sent.contains_key(n)
    });
    let dir = a.get("direction").unwrap_or(if sp.hermes_export.is_some() {
        "both"
    } else {
        "g2h"
    });
    if !["both", "g2h", "h2g"].contains(&dir) {
        return Err(format!("--direction must be both, g2h or h2g (got {dir})"));
    }
    let h2g_out = a
        .path("out")
        .or(sp.hermes_export.as_ref().and_then(|h| h.out.clone()));
    let o = adapters::hermes::Opts {
        spec: sp,
        profile: profile.clone(),
        real_home,
        hermes_bin: a.get("hermes-bin").unwrap_or("hermes").to_string(),
        hermes_home: (hh != home().join(".hermes")).then(|| hh.clone()),
    };
    let target = HermesProfile;
    let c = Ctx::new(&m, &src, &dest, &o);
    if cmd == "export-hermes" || (cmd == "sync" && dir == "h2g") || (cmd == "drift" && dir == "h2g")
    {
        return h2g_report(
            cmd,
            &c,
            h2g_out.as_deref(),
            cmd != "drift" && !a.has("dry-run"),
            None,
        );
    }
    // Pre-flight: the report an apply prints is the plan's items plus paths under `dest`; guard it before
    // anything is written, so a refused report never follows a half-done apply.
    let preflight = || -> Res<()> {
        let items = target.plan(&m, &src, &dest, &o)?.items;
        // cron goes through the hermes CLI: check it starts before the first file is written
        if c.real_home
            && !a.has("dry-run")
            && items
                .iter()
                .any(|i| i.area == "cron" && matches!(i.status, St::Create | St::Update))
        {
            doctor::runnable(&c.hermes_bin)
                .map_err(|e| format!("cron items need hermes: {e} (or pass --no-cron)"))?;
        }
        let r = profile_report(
            cmd,
            &profile,
            &dest,
            &src,
            items,
            Some(dest.join(sync::BACKUPS).join("restore.zip")),
            false,
            a.has("dry-run"),
        );
        botport::core::secrets::guard("the report", json_sorted(&r.json).as_bytes()).and_then(
            |_| botport::core::secrets::guard("the summary", r.lines.join("\n").as_bytes()),
        )
    };
    let (items, rp, restart) = match cmd {
        "plan" | "drift" => (target.plan(&m, &src, &dest, &o)?.items, None, false),
        "sync" => {
            let p = target.plan(&m, &src, &dest, &o)?;
            let unchanged = !sync::has_legacy_names(&dest)
                && p.synced_digest == Some(src.digest())
                && !p.items.iter().any(|i| i.status.is_change());
            if unchanged {
                (p.items, None, false)
            } else {
                preflight()?;
                let r = target.apply(&m, &src, &dest, &o, a.has("adopt"), a.has("dry-run"))?;
                (r.items, r.restore_point, r.restart_needed)
            }
        }
        _ => {
            preflight()?;
            let r = target.apply(&m, &src, &dest, &o, a.has("adopt"), a.has("dry-run"))?;
            (r.items, r.restore_point, r.restart_needed)
        }
    };
    let r = profile_report(
        cmd,
        &profile,
        &dest,
        &src,
        items,
        rp,
        restart,
        a.has("dry-run"),
    );
    if dir == "both" && matches!(cmd, "sync" | "drift") {
        return h2g_report(
            cmd,
            &c,
            h2g_out.as_deref(),
            cmd == "sync" && !a.has("dry-run"),
            Some(r),
        );
    }
    Ok(r)
}

/// Hermes -> Grok: collect what the Grok side lacks; write it (and record exported skills) when `write`.
/// With `g2h` (the Grok -> Hermes report of this run) both directions go into one report and one first line.
fn h2g_report(
    cmd: &str,
    c: &Ctx,
    out: Option<&std::path::Path>,
    write: bool,
    g2h: Option<Report>,
) -> Res<Report> {
    let e = twoway::collect(c)?;
    let pending: Vec<&Item> = e
        .items
        .iter()
        .filter(|i| matches!(i.status, St::Create | St::Conflict))
        .collect();
    let conflicts = e.items.iter().filter(|i| i.status == St::Conflict).count();
    let mut wrote = None;
    if write && pending.iter().any(|i| i.status == St::Create) {
        let o =
            out.ok_or("the Hermes -> Grok bundle needs --out or [hermes_export] out in the spec")?;
        twoway::write(&e, c, o)?;
        twoway::record(&e, c)?;
        wrote = Some(o.to_path_buf());
    } else if write {
        twoway::record(&e, c)?; // re-baseline resolved conflicts even when nothing new is sent
    }
    let what = format!(
        "{} fact(s), {} skill(s), {} cron",
        e.facts.len(),
        e.skills.len(),
        e.cron.len()
    );
    let h2g_head = if pending.is_empty() {
        "h2g IN SYNC".to_string()
    } else if conflicts > 0 {
        format!(
            "h2g {} pending ({what}), {conflicts} conflict",
            pending.len() - conflicts
        )
    } else {
        format!("h2g {} pending ({what})", pending.len())
    };
    let mut lines = vec![];
    let mut code: u8 = match cmd {
        "drift" => u8::from(!pending.is_empty()),
        _ if conflicts > 0 => 3,
        _ => 0,
    };
    let mut j = json!({"command": cmd, "profile": c.profile, "h2g": {"items": e.items, "facts": e.facts.len(),
        "skills": e.skills.keys().collect::<Vec<_>>(), "cron": e.cron.len(), "bundle": wrote}});
    match g2h {
        Some(g) => {
            // "drift demo: IN SYNC (...)" -> "drift demo: g2h IN SYNC (...); h2g ..."
            let first = g.lines.first().cloned().unwrap_or_default();
            let (pre, rest) = first.split_once(": ").unwrap_or((&first, ""));
            lines.push(format!("{pre}: g2h {rest}; {h2g_head}"));
            lines.extend(
                g.lines
                    .iter()
                    .skip(1)
                    .filter(|l| !l.starts_with("details:"))
                    .cloned(),
            );
            code = code.max(g.code);
            let mut gj = g.json;
            gj["h2g"] = j["h2g"].take();
            gj["in_sync"] = json!(gj["in_sync"] == json!(true) && pending.is_empty());
            j = gj;
        }
        None => lines.push(format!("{cmd} {}: {h2g_head}", c.profile)),
    }
    for i in e
        .items
        .iter()
        .filter(|i| i.status == St::Conflict || i.status == St::Skip)
    {
        lines.push(format!(
            "  {} {} {}: {}",
            i.status.word(),
            i.area,
            i.name,
            i.detail
        ));
    }
    if let Some(o) = &wrote {
        lines.push(format!(
            "h2g bundle: {} ({what}; the Grok bot loads it with docs/AGENT-IMPORT.md)",
            o.display()
        ));
    }
    Ok(Report {
        ok: code == 0,
        code,
        lines,
        json: j,
    })
}

#[allow(clippy::too_many_arguments)]
fn profile_report(
    cmd: &str,
    profile: &str,
    dest: &std::path::Path,
    src: &Source,
    items: Vec<Item>,
    rp: Option<PathBuf>,
    restart: bool,
    dry: bool,
) -> Report {
    let count = |s: St| items.iter().filter(|i| i.status == s).count();
    // Skip = deliberately not managed here (e.g. cron off ~/.hermes): reported, but not a difference.
    // For drift, the manual checklist file is advice, not synced state: it is on the `manual:` line instead.
    let checklist = |i: &Item| cmd == "drift" && i.area == "manual";
    let diffs: Vec<&Item> = items
        .iter()
        .filter(|i| !matches!(i.status, St::InSync | St::Skip | St::Manual) && !checklist(i))
        .collect();
    let conflicts = count(St::Conflict);
    let changes = items.iter().filter(|i| i.status.is_change()).count();
    let applied = matches!(cmd, "apply" | "sync") && !dry && rp.is_some();
    let head = if diffs.is_empty() {
        format!(
            "{cmd} {profile}: IN SYNC ({} items, source {})",
            items.len(),
            short(&src.digest())
        )
    } else {
        let mut parts = vec![];
        for s in [
            St::Create,
            St::Update,
            St::Retire,
            St::Conflict,
            St::Duplicate,
            St::Skip,
        ] {
            let n = if s == St::Skip {
                count(s)
            } else {
                diffs.iter().filter(|i| i.status == s).count()
            };
            if n > 0 {
                parts.push(format!("{n} {}", s.word()));
            }
        }
        format!(
            "{cmd} {profile}: {}{} ({} items, source {})",
            if applied { "applied: " } else { "" },
            parts.join(", "),
            items.len(),
            short(&src.digest())
        )
    };
    // plan/apply/sync say where they write, so a stray HERMES_HOME is visible before anything changes
    let head = match head.split_once(": ") {
        Some((who, rest)) if matches!(cmd, "plan" | "apply" | "sync") => {
            format!("{who} ({}): {rest}", dest.display())
        }
        _ => head,
    };
    let mut lines = vec![head];
    if count(St::Manual) > 0 {
        lines.push(format!(
            "manual: {} item(s) to set up by hand (names only): {}",
            count(St::Manual),
            sync::MANUAL
        ));
    }
    if let Some(p) = &rp {
        lines.push(format!(
            "restore point: {}",
            p.strip_prefix(dest).unwrap_or(p).display()
        ));
    }
    if restart {
        lines.push(format!("config.yaml changed: restart the gateway when idle (hermes -p {profile} gateway restart)"));
    }
    // Token-lean: one line per area for ordinary changes (names only), one line per conflict/skip (needs a human).
    let mut areas: Vec<&str> = vec![];
    for i in &diffs {
        if !areas.contains(&i.area.as_str()) {
            areas.push(&i.area);
        }
    }
    for area in areas {
        let mut parts = vec![];
        for s in [St::Create, St::Update, St::Retire] {
            let names: Vec<&str> = diffs
                .iter()
                .filter(|i| i.area == area && i.status == s)
                .map(|i| i.name.rsplit_once('/').map(|x| x.1).unwrap_or(&i.name))
                .collect();
            if !names.is_empty() {
                parts.push(format!("{} {}", s.word(), names.join(" ")));
            }
        }
        if !parts.is_empty() {
            lines.push(format!("  {area}: {}", parts.join("; ")));
        }
        for i in diffs.iter().filter(|i| {
            i.area == area && matches!(i.status, St::Conflict | St::Duplicate | St::Skip)
        }) {
            lines.push(format!(
                "  {} {area} {}: {}",
                i.status.word(),
                i.name,
                i.detail
            ));
        }
    }
    let code = match cmd {
        "drift" => u8::from(!diffs.is_empty()),
        "apply" | "sync" if conflicts > 0 => 3,
        _ => 0,
    };
    let _ = changes;
    Report {
        ok: code == 0,
        code,
        lines,
        json: json!({"command": cmd, "profile": profile, "dest": dest,
        "bundle": src.path, "agent_id": src.id, "bundle_digest": src.digest(), "in_sync": diffs.is_empty(), "items": items,
        "restore_point": rp, "restart_needed": restart}),
    }
}

fn cmd_test(a: &Args) -> Res<Report> {
    let profile = a.get("profile").ok_or("--profile is required")?;
    plain_name("profile", profile)?;
    let bp = a.path("battery").ok_or("--battery is required")?;
    let b = battery::load(&bp)?;
    let only: Vec<String> = a
        .get("only")
        .map(|s| {
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let out = a
        .path("out")
        .unwrap_or_else(|| std::env::temp_dir().join(format!("botport-battery-{profile}")));
    let wd = out.join("workdir");
    std::fs::create_dir_all(&wd).map_err(|e| e.to_string())?;
    let res = battery::run(
        a.get("hermes-bin").unwrap_or("hermes"),
        profile,
        &b,
        &only,
        &wd,
    )?;
    let pass = res.iter().filter(|r| r["pass"] == json!(true)).count();
    let jl: String = res.iter().map(jsonl_line).collect();
    let file = out.join(format!("battery-{}.jsonl", utc_stamp()));
    write_atomic(&file, jl.as_bytes(), Some(0o644))?;
    let mut lines = vec![format!("test {profile}: {pass}/{} pass", res.len())];
    // A pass/fail table: failures first, then passes, cut to fit the 10-line budget.
    let mut rows: Vec<String> = res
        .iter()
        .filter(|r| r["pass"] != json!(true))
        .map(|r| {
            let why: Vec<&str> = r["why"]
                .as_array()
                .map(|w| w.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!(
                "  FAIL {} — {}",
                r["id"].as_str().unwrap_or(""),
                why.join("; ")
            )
        })
        .collect();
    rows.extend(
        res.iter()
            .filter(|r| r["pass"] == json!(true))
            .map(|r| format!("  PASS {}", r["id"].as_str().unwrap_or(""))),
    );
    if rows.len() > 8 {
        let more = rows.len() - 7;
        rows.truncate(7);
        rows.push(format!("  … {more} more in the results file"));
    }
    lines.extend(rows);
    lines.push(format!("results: {}", file.display()));
    let ok = pass == res.len();
    Ok(Report {
        ok,
        code: u8::from(!ok),
        lines,
        json: json!({"command": "test", "profile": profile, "pass": pass,
        "total": res.len(), "results": res, "file": file}),
    })
}

fn cmd_battery_init(a: &Args) -> Res<Report> {
    let dir = a
        .path("bundle")
        .or_else(|| a.pos.get(2).map(PathBuf::from))
        .ok_or(
            "battery init needs a bundle directory or grok-bot-export file (PATH or --bundle)",
        )?;
    let out = a.path("out").ok_or("--out is required")?;
    if !dir.is_dir() && !adapters::grok_bot::export_json::is_export(&dir) {
        return Err(format!(
            "battery init: {} is not a bundle directory or a grok-bot-export v1 file",
            dir.display()
        ));
    }
    let (mut m, mut src) = GrokBot.read_source(&dir)?;
    if let Some(p) = a.path("spec") {
        let s = spec::Spec::load(
            &p,
            &a.path("hermes-home").unwrap_or_else(default_hermes_home),
        )?;
        retain_skills(&mut m, &mut src, |n| s.skill_allowed(n));
    }
    let nc = std::fs::read_to_string(dir.join("NOT_CAPTURED.md")).unwrap_or_default();
    let v = battery::init(&m, &src, &nc);
    write_atomic(&out, json_sorted(&v).as_bytes(), Some(0o644))?;
    let mut fams = std::collections::BTreeMap::<&str, usize>::new();
    for c in v["cases"].as_array().into_iter().flatten() {
        *fams.entry(c["family"].as_str().unwrap_or("")).or_default() += 1;
    }
    let parts: Vec<String> = fams.iter().map(|(f, n)| format!("{f} {n}")).collect();
    let lines = vec![
        format!(
            "battery init: {} cases ({})",
            v["cases"].as_array().map_or(0, Vec::len),
            parts.join(", ")
        ),
        format!(
            "wrote {} — edit it, then: botport test --profile NAME --battery FILE",
            out.display()
        ),
    ];
    Ok(Report {
        ok: true,
        code: 0,
        lines,
        json: json!({"command": "battery init", "out": out, "families": fams}),
    })
}

/// `$HERMES_HOME` when set (as the hermes CLI does), else `~/.hermes`.
fn default_hermes_home() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".hermes"))
}

fn cmd_doctor(a: &Args) -> Res<Report> {
    let hh = a.path("hermes-home").unwrap_or_else(default_hermes_home);
    let cs = doctor::run(a, &hh);
    let ok = cs.iter().all(|c| c.ok);
    let (lines, json) = doctor::lines_json(&cs);
    Ok(Report {
        ok,
        code: if ok { 0 } else { 1 },
        lines,
        json,
    })
}

//! docs/schema/botmodel-v1.json describes `BotModel` as it is: every committed model fixture and every model the
//! adapters read from their conformance fixtures validates against it, and so does a model with every field set
//! (so a field added to the types but not to the schema fails here, via `additionalProperties: false`).
use botport::adapters::{grok_bot::GrokBot, hermes::HermesProfile};
use botport::core::adapter::Adapter;
use botport::core::model::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn schema() -> Value {
    let s = std::fs::read_to_string(root().join("docs/schema/botmodel-v1.json")).unwrap();
    serde_json::from_str(&s).unwrap()
}

/// The keywords `validate` understands; the schema may use no others (so nothing in it goes unchecked).
const KEYWORDS: [&str; 13] = [
    "$schema",
    "$id",
    "$ref",
    "$defs",
    "title",
    "description",
    "type",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "enum",
    "pattern",
];

fn resolve<'a>(root: &'a Value, s: &'a Value) -> &'a Value {
    match s.get("$ref").and_then(Value::as_str) {
        Some(r) => &root["$defs"][r.strip_prefix("#/$defs/").expect("local $ref")],
        None => s,
    }
}

/// A small JSON Schema validator for the keywords above; pushes one message per violation.
fn validate(root: &Value, s: &Value, v: &Value, at: &str, errs: &mut Vec<String>) {
    let s = resolve(root, s);
    if let Some(t) = s.get("type").and_then(Value::as_str) {
        let ok = match t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "boolean" => v.is_boolean(),
            other => panic!("type {other} not supported by this validator"),
        };
        if !ok {
            errs.push(format!("{at}: not a {t}"));
            return;
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            errs.push(format!("{at}: {v} not in {e:?}"));
        }
    }
    if let (Some(p), Some(x)) = (s.get("pattern").and_then(Value::as_str), v.as_str()) {
        if !regex::Regex::new(p).unwrap().is_match(x) {
            errs.push(format!("{at}: {x:?} does not match {p}"));
        }
    }
    if let (Some(items), Some(a)) = (s.get("items"), v.as_array()) {
        for (i, x) in a.iter().enumerate() {
            validate(root, items, x, &format!("{at}[{i}]"), errs);
        }
    }
    let Some(o) = v.as_object() else { return };
    for r in s
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !o.contains_key(r.as_str().unwrap()) {
            errs.push(format!("{at}: missing {r}"));
        }
    }
    let props = s.get("properties").and_then(Value::as_object);
    for (k, x) in o {
        let p = format!("{at}.{k}");
        match (props.and_then(|p| p.get(k)), s.get("additionalProperties")) {
            (Some(ps), _) => validate(root, ps, x, &p, errs),
            (None, Some(Value::Bool(false))) => errs.push(format!("{p}: not in the schema")),
            (None, Some(ap @ Value::Object(_))) => validate(root, ap, x, &p, errs),
            (None, _) => {}
        }
    }
}

fn check(v: &Value, what: &str) {
    let root = schema();
    let mut errs = Vec::new();
    validate(&root, &root, v, "$", &mut errs);
    assert!(
        errs.is_empty(),
        "{what} does not match the schema:\n{}",
        errs.join("\n")
    );
}

fn check_model(m: &BotModel, what: &str) {
    check(&serde_json::from_str(&m.to_json()).unwrap(), what);
}

#[test]
fn schema_uses_only_validated_keywords() {
    fn walk(v: &Value, under_props: bool) {
        if let Some(o) = v.as_object() {
            for (k, x) in o {
                if !under_props {
                    assert!(KEYWORDS.contains(&k.as_str()), "keyword {k} is not checked");
                }
                // property names and $defs names are not keywords; their values are schemas
                let named = matches!(k.as_str(), "properties" | "$defs");
                if named {
                    x.as_object().unwrap().values().for_each(|s| walk(s, false));
                } else if !under_props && matches!(k.as_str(), "items" | "additionalProperties") {
                    walk(x, false);
                }
            }
        }
    }
    walk(&schema(), false);
}

#[test]
fn committed_model_fixtures_match() {
    let dir = root().join("tests/fixtures/model");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "json") {
            let src = std::fs::read_to_string(&p).unwrap();
            let what = p.file_name().unwrap().to_string_lossy().to_string();
            check(&serde_json::from_str(&src).unwrap(), &what);
            // and the types read it back to the same document
            check_model(&BotModel::from_json(&src).unwrap(), &what);
            n += 1;
        }
    }
    assert!(n >= 4, "expected the model fixtures, found {n}");
}

fn conformance(id: &str) -> PathBuf {
    root().join("tests/fixtures/conformance").join(id)
}

fn read_and_write_back<A: Adapter>(a: &A, fixture: &Path) {
    let m = a.read(fixture).unwrap();
    check_model(&m, &format!("{} read", a.id()));
    let t = std::env::temp_dir().join(format!("botport-schema-{}-{}", a.id(), std::process::id()));
    std::fs::create_dir_all(&t).unwrap();
    a.write(&m, &t).unwrap();
    let back = a.read(&t);
    std::fs::remove_dir_all(&t).unwrap();
    check_model(&back.unwrap(), &format!("{} write -> read", a.id()));
}

#[test]
fn conformance_fixture_models_match() {
    read_and_write_back(&GrokBot, &conformance("grok-bot"));
    read_and_write_back(&HermesProfile, &conformance("hermes"));
}

/// Every field set. A struct literal (no `..Default`): a new field in the types breaks this until it is added
/// here, and then the schema (`additionalProperties: false`) rejects it until it is described there too.
fn every_field() -> BotModel {
    BotModel {
        schema: "bot-model/1".into(),
        persona: Persona {
            name: "n".into(),
            instructions: "i".into(),
        },
        memory: vec![Fact {
            text: "t".into(),
            tier: Some("core".into()),
            scope: Some("user".into()),
            source: Some("s".into()),
            date: Some("2026-01-01".into()),
            origin_side: Some("a".into()),
        }],
        memory_text: Some("# m\n".into()),
        skills: vec![Skill {
            name: "k".into(),
            description: Some("d".into()),
            files: BTreeMap::from([("SKILL.md".into(), "x".into())]),
            binary_files: BTreeMap::from([("a.png".into(), "89ff00".into())]),
        }],
        schedules: vec![Schedule {
            name: "s".into(),
            cron: Some("0 8 * * *".into()),
            tz: "UTC".into(),
            kind: ScheduleKind::Event,
            action: "a".into(),
            enabled: true,
        }],
        connectors: vec![Connector {
            name: "c".into(),
            kind: "mcp-http".into(),
            url: Some("https://example.invalid/mcp".into()),
            tools: vec!["t".into()],
        }],
        credentials: vec![CredentialRef::new("TOKEN_NAME")],
        history: Some(HistoryRef {
            kind: "jsonl".into(),
            path: "h.jsonl".into(),
        }),
        manual: vec![ManualItem {
            item: "schedule s".into(),
            todo: "by hand".into(),
            done: true,
        }],
        extras: BTreeMap::from([("x".into(), json!({"any": [1, null]}))]),
    }
}

#[test]
fn a_model_with_every_field_set_matches() {
    let m = every_field();
    let v: Value = serde_json::from_str(&m.to_json()).unwrap();
    let mut keys = v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    keys.sort();
    let mut want = schema()["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    want.sort();
    assert_eq!(keys, want, "top-level fields of the types vs the schema");
    check(&v, "every field set");
}

#[test]
fn the_schema_rejects_what_the_types_reject() {
    let mut v: Value = serde_json::from_str(&every_field().to_json()).unwrap();
    v["credentials"][0]["value"] = json!("not-a-real-secret");
    let root = schema();
    let mut errs = Vec::new();
    validate(&root, &root, &v, "$", &mut errs);
    assert_eq!(errs, ["$.credentials[0].value: not in the schema"]);
    v["credentials"][0].as_object_mut().unwrap().remove("value");
    v["schema"] = json!("bot-model/2");
    errs.clear();
    validate(&root, &root, &v, "$", &mut errs);
    assert_eq!(errs.len(), 1, "{errs:?}");
}

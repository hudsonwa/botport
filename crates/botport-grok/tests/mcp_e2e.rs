//! End to end: every tool the `botport grok mcp` server exposes, called over the JSON-RPC stdio loop
//! (`mcp::serve`) against the fake transport. One test function, because the file tools confine paths to the
//! server's working folder (the process cwd), which this test points at a scratch folder.
//!
//! The contract checked here (src/mcp.rs):
//! - every request with an `id` gets exactly one reply with that id; a notification (no id) gets none; a line
//!   that is not JSON gets a -32700 error with id null;
//! - an unknown method is -32601; an unknown or unlisted tool, a missing argument, an unknown action, an action
//!   not offered on a bot-callable server, or a path outside the server's folder is -32602 (a protocol error,
//!   nothing ran);
//! - otherwise the reply is a `result` with `structuredContent` (holding `exit`) and `content[0].text` (the same
//!   JSON as text); `isError` is false exactly when `exit` is 0 (done), 2 (still working) or 3 (needs the user);
//! - an engine that cannot be made answers the tool call with `isError: true` and the failure as content.
mod common;
use botport_grok::mcp;
use botport_grok::policy::{Caller, Policy};
use botport_grok::transport::fake::FakeTransport;
use common::*;
use serde_json::{json, Value};

const BOT2: &str = "00000000-0000-4000-8000-000000000002";

fn session(
    build: impl Fn(FakeTransport) -> FakeTransport,
    bot_callable: bool,
    lines: &[Value],
) -> Vec<Value> {
    session_raw(
        build,
        bot_callable,
        &lines.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
    )
}

fn session_raw(
    build: impl Fn(FakeTransport) -> FakeTransport,
    bot_callable: bool,
    lines: &[String],
) -> Vec<Value> {
    let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mut out = Vec::new();
    let mut make = || {
        let clk = clock();
        let f = build(FakeTransport::load(&fx(), clk.clone()).unwrap());
        Ok(engine_from(
            f,
            clk,
            Policy::default(),
            Caller::Mcp { bot_callable },
        ))
    };
    let mut status = || json!({"exit": 0, "proof": "PASS: status; self-check", "running": true});
    mcp::serve(
        &mut make,
        &mut status,
        input.as_bytes(),
        &mut out,
        bot_callable,
    )
    .unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
}

/// One tool call in a fresh session; returns the reply after checking the result envelope contract.
fn one(
    build: impl Fn(FakeTransport) -> FakeTransport,
    bot_callable: bool,
    name: &str,
    args: Value,
) -> Value {
    let r = session(build, bot_callable, &[call(7, name, args.clone())]);
    assert_eq!(r.len(), 1, "{name}: one reply");
    assert_eq!(r[0]["jsonrpc"], "2.0");
    assert_eq!(r[0]["id"], 7);
    if let Some(res) = r[0].get("result") {
        let sc = &res["structuredContent"];
        let exit = sc["exit"]
            .as_u64()
            .unwrap_or_else(|| panic!("{name} {args}: no exit in {sc}"));
        assert_eq!(
            res["isError"],
            json!(!matches!(exit, 0 | 2 | 3)),
            "{name} {args}: isError follows exit {exit}"
        );
        let text: Value =
            serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, sc, "{name}: text content mirrors structuredContent");
    }
    r[0].clone()
}

fn id(f: FakeTransport) -> FakeTransport {
    f
}

fn exit_of(r: &Value) -> u64 {
    r["result"]["structuredContent"]["exit"]
        .as_u64()
        .unwrap_or_else(|| panic!("not a result: {r}"))
}

fn proto_err(r: &Value) -> i64 {
    r["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("not an error: {r}"))
}

#[test]
fn every_mcp_tool_end_to_end_over_the_fake_transport() {
    let s = Scratch::new("mcp-e2e");
    std::env::set_current_dir(&s.0).unwrap();
    std::fs::write(s.0.join("up.txt"), b"hello from the e2e test").unwrap();

    // ------------------------------------------------ protocol
    let r = session_raw(
        id,
        false,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})
                .to_string(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
            "this is not json".into(),
            String::new(),
            json!({"jsonrpc": "2.0", "id": "s-2", "method": "tools/list"}).to_string(),
            json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}).to_string(),
            json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}).to_string(),
            call(5, "grok_explode", json!({})).to_string(),
        ],
    );
    assert_eq!(r.len(), 6, "{r:?}");
    assert_eq!(
        r[0]["result"]["protocolVersion"], "2025-03-26",
        "echoes the client's version"
    );
    assert_eq!(r[0]["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(
        (r[1]["id"].clone(), proto_err(&r[1])),
        (Value::Null, -32700)
    );
    assert_eq!(r[2]["id"], "s-2", "string ids are echoed");
    let mut names: Vec<String> = r[2]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    let all = [
        "grok_ask",
        "grok_bots",
        "grok_file_delete",
        "grok_files",
        "grok_read",
        "grok_routine_remove",
        "grok_routines",
        "grok_send",
        "grok_status",
        "grok_stop",
        "grok_usage",
        "grok_wait",
    ];
    assert_eq!(names, all);
    for t in r[2]["result"]["tools"].as_array().unwrap() {
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        for req in schema["required"].as_array().unwrap() {
            assert!(
                schema["properties"].get(req.as_str().unwrap()).is_some(),
                "{t}"
            );
        }
    }
    assert_eq!(r[3]["result"], json!({}));
    assert_eq!(proto_err(&r[4]), -32601);
    assert_eq!(proto_err(&r[5]), -32602);

    // ------------------------------------------------ every tool, success path
    assert_eq!(exit_of(&one(id, false, "grok_status", json!({}))), 0);
    let b = one(id, false, "grok_bots", json!({}));
    assert_eq!(exit_of(&b), 0);
    assert_eq!(
        b["result"]["structuredContent"]["bots"]
            .as_array()
            .unwrap()
            .len(),
        7
    );

    let a = one(
        |f| f.with_turn("reply-ready"),
        false,
        "grok_ask",
        json!({"bot": "main", "message": "hi"}),
    );
    assert_eq!(exit_of(&a), 0);
    assert_eq!(
        a["result"]["structuredContent"]["messages"][0]["text"],
        "READY"
    );
    let a = one(
        |f| f.with_turn("needs-approval"),
        false,
        "grok_ask",
        json!({"bot": "main", "message": "go"}),
    );
    assert_eq!(exit_of(&a), 3, "waiting for the user is not an error");

    let r = session(
        |f| f.with_turn("reply-ready"),
        false,
        &[
            call(1, "grok_send", json!({"bot": "main", "message": "hi"})),
            call(
                2,
                "grok_wait",
                json!({"bot": "main", "timeout_s": 60, "settle_s": 2}),
            ),
        ],
    );
    assert_eq!(exit_of(&r[0]), 0, "{}", r[0]);
    assert_eq!(exit_of(&r[1]), 0, "{}", r[1]);
    assert_eq!(
        r[1]["result"]["structuredContent"]["messages"][0]["text"],
        "READY"
    );

    let rd = one(id, false, "grok_read", json!({"bot": "main", "limit": 2}));
    assert_eq!(exit_of(&rd), 0);
    assert_eq!(
        rd["result"]["structuredContent"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let u = one(id, false, "grok_usage", json!({}));
    assert_eq!(exit_of(&u), 0);
    assert!(u["result"]["structuredContent"]["percent_used"].is_number());

    // routines: list, disable, enable, add, run, remove
    let l = one(id, false, "grok_routines", json!({"bot": MAIN}));
    assert_eq!(exit_of(&l), 0);
    assert_eq!(
        l["result"]["structuredContent"]["routines"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let r = session(
        id,
        false,
        &[
            call(
                1,
                "grok_routines",
                json!({"bot": MAIN, "action": "disable", "routine": "weekly-cost-log"}),
            ),
            call(2, "grok_routines", json!({"bot": MAIN, "action": "list"})),
            call(
                3,
                "grok_routines",
                json!({"bot": MAIN, "action": "enable", "routine": "weekly-cost-log"}),
            ),
        ],
    );
    assert_eq!(exit_of(&r[0]), 0, "{}", r[0]);
    assert_eq!(
        r[1]["result"]["structuredContent"]["routines"][0]["enabled"],
        false
    );
    assert_eq!(exit_of(&r[2]), 0, "{}", r[2]);
    let add = one(
        |f| f.with_turn("reply-ready"),
        false,
        "grok_routines",
        json!({"bot": BOT2, "action": "add", "name": "e2e probe", "schedule": "0 9 * * 1", "tz": "UTC",
               "prompt": "Write SYNTHETIC OK.", "timeout_s": 60, "settle_s": 2}),
    );
    assert_eq!(exit_of(&add), 0, "{add}");
    let run = one(
        |f| f.with_turn("reply-ready"),
        false,
        "grok_routines",
        json!({"bot": MAIN, "action": "run", "routine": "weekly-cost-log", "timeout_s": 60, "settle_s": 2}),
    );
    assert_eq!(exit_of(&run), 0, "{run}");
    let rm = one(
        id,
        false,
        "grok_routine_remove",
        json!({"bot": MAIN, "routine": "weekly-cost-log", "confirm": "weekly-cost-log"}),
    );
    assert_eq!(exit_of(&rm), 0, "{rm}");
    let rm = one(
        id,
        false,
        "grok_routine_remove",
        json!({"bot": MAIN, "routine": "weekly", "confirm": "weekly"}),
    );
    assert_eq!(
        exit_of(&rm),
        4,
        "a non-exact routine is refused (floor 4) and is an error: {rm}"
    );

    // files: list, download, fetch, upload, delete
    let fl = one(id, false, "grok_files", json!({"bot": BOT2}));
    assert_eq!(exit_of(&fl), 0);
    assert_eq!(
        fl["result"]["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let dl = one(
        id,
        false,
        "grok_files",
        json!({"bot": BOT2, "action": "download", "names": ["export.json"], "out": "dl"}),
    );
    assert_eq!(exit_of(&dl), 0, "{dl}");
    assert_eq!(
        std::fs::read(s.0.join("dl/export.json")).unwrap().len(),
        4096
    );
    let fe = one(
        |f| f.with_attachment(MAIN, "sent.json", br#"{"ok":true}"#.to_vec()),
        false,
        "grok_files",
        json!({"bot": MAIN, "action": "fetch", "out": "dl"}),
    );
    assert_eq!(exit_of(&fe), 0, "{fe}");
    assert_eq!(
        std::fs::read(s.0.join("dl/sent.json")).unwrap(),
        br#"{"ok":true}"#
    );
    let up = one(
        id,
        false,
        "grok_files",
        json!({"bot": BOT2, "action": "upload", "paths": ["up.txt"]}),
    );
    assert_eq!(exit_of(&up), 0, "{up}");
    let del = one(
        id,
        false,
        "grok_file_delete",
        json!({"bot": BOT2, "name": "export.json", "confirm": "export.json"}),
    );
    assert_eq!(exit_of(&del), 0, "{del}");
    let del = one(
        id,
        false,
        "grok_file_delete",
        json!({"bot": BOT2, "name": "export", "confirm": "export"}),
    );
    assert_eq!(exit_of(&del), 4, "inexact name refused: {del}");

    let st = one(
        |f| f.with_turn("steer-stop"),
        false,
        "grok_stop",
        json!({"bot": "main"}),
    );
    assert!(st.get("result").is_some(), "{st}");
    let r = session(
        |f| f.with_turn("steer-stop"),
        false,
        &[
            call(
                1,
                "grok_send",
                json!({"bot": "main", "message": "long job"}),
            ),
            call(
                2,
                "grok_stop",
                json!({"bot": "main", "steer": "please stop"}),
            ),
        ],
    );
    assert_eq!(exit_of(&r[1]), 0, "{}", r[1]);

    // ------------------------------------------------ argument and path errors (-32602, nothing ran)
    for (name, args) in [
        ("grok_ask", json!({"bot": "main"})),
        ("grok_send", json!({"message": "hi"})),
        ("grok_wait", json!({})),
        ("grok_read", json!({"bot": 7})),
        ("grok_routines", json!({"bot": MAIN, "action": "explode"})),
        (
            "grok_routines",
            json!({"bot": MAIN, "action": "add", "name": "x"}),
        ),
        ("grok_routines", json!({"bot": MAIN, "action": "enable"})),
        (
            "grok_routine_remove",
            json!({"bot": MAIN, "routine": "weekly-cost-log"}),
        ),
        ("grok_files", json!({"bot": BOT2, "action": "explode"})),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "download", "out": "../escape"}),
        ),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "download", "out": ".hidden"}),
        ),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "download", "out": "/"}),
        ),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "upload", "paths": ["missing.txt"]}),
        ),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "upload", "paths": ["/etc/hosts"]}),
        ),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "upload", "paths": ["dl"]}),
        ),
        (
            "grok_file_delete",
            json!({"bot": BOT2, "name": "export.json"}),
        ),
        ("grok_stop", json!({})),
    ] {
        let r = one(id, false, name, args.clone());
        assert_eq!(proto_err(&r), -32602, "{name} {args}: {r}");
    }

    // ------------------------------------------------ bot-callable server
    let r = session(
        id,
        true,
        &[json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})],
    );
    let tools = r[0]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 9);
    for t in tools {
        assert!(!["grok_stop", "grok_routine_remove", "grok_file_delete"]
            .contains(&t["name"].as_str().unwrap()));
        if let Some(e) = t["inputSchema"]["properties"]["action"]["enum"].as_array() {
            assert_eq!(e, &vec![json!("list")], "{}", t["name"]);
        }
    }
    assert_eq!(
        exit_of(&one(id, true, "grok_routines", json!({"bot": MAIN}))),
        0
    );
    assert_eq!(
        exit_of(&one(
            id,
            true,
            "grok_files",
            json!({"bot": BOT2, "action": "list"})
        )),
        0
    );
    for (name, args) in [
        ("grok_stop", json!({"bot": "main"})),
        (
            "grok_routine_remove",
            json!({"bot": MAIN, "routine": "weekly-cost-log", "confirm": "weekly-cost-log"}),
        ),
        (
            "grok_file_delete",
            json!({"bot": BOT2, "name": "export.json", "confirm": "export.json"}),
        ),
        (
            "grok_routines",
            json!({"bot": MAIN, "action": "disable", "routine": "weekly-cost-log"}),
        ),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "upload", "paths": ["up.txt"]}),
        ),
        (
            "grok_files",
            json!({"bot": BOT2, "action": "download", "out": "dl2"}),
        ),
    ] {
        let r = one(id, true, name, args.clone());
        assert_eq!(proto_err(&r), -32602, "{name} {args}: {r}");
    }
    assert!(
        !s.0.join("dl2").exists(),
        "a refused download wrote nothing"
    );

    // ------------------------------------------------ an engine that cannot be made
    let mut out = Vec::new();
    let mut make = || Err(json!({"exit": 5, "proof": "FAIL: transport; no debug port"}));
    let mut status = || json!({"exit": 5});
    let input = format!(
        "{}\n{}\n",
        call(1, "grok_bots", json!({})),
        call(2, "grok_status", json!({}))
    );
    mcp::serve(&mut make, &mut status, input.as_bytes(), &mut out, false).unwrap();
    let r: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(r[0]["result"]["isError"], true);
    assert_eq!(r[0]["result"]["structuredContent"]["exit"], 5);
    assert_eq!(
        r[1]["result"]["isError"], true,
        "status with exit 5 is an error"
    );

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
}

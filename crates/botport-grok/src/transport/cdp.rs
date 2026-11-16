//! The debug-port transport (app 1.1.0): one loopback DevTools connection to the app's single `page` target.
//!
//! - Reads: one `Runtime.evaluate` per read with a fixed snippet that walks React fibers from a DOM element to the
//!   first props object holding the field (`agents`+`listRows` for the sidebar; `entries`+`isAgentRunning` for the
//!   chat). The raw props go through [`crate::parse`].
//! - Second route: the sidebar rows' DOM attributes (`data-agent-id`, `aria-label`, `aria-description`,
//!   `data-awaiting`, `data-composing`, `data-layout`).
//! - Writes: `Input.*` only on elements found by attribute (the bot row, the composer). No coordinates from config.
//! - IPC goes through [`CdpTransport::ipc`], which refuses every method off the allowlist (floor 6).
//! - CDP methods are allowlisted too: `Runtime.evaluate` and `Input.insertText|dispatchKeyEvent|dispatchMouseEvent`.
use super::ws::{http_get, Ws};
use super::{Cap, Capabilities, TResult, Transport, TransportError};
use crate::lifecycle::{AppHost, PortFile};
use crate::model::*;
use crate::parse;
use crate::policy::floors;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

const CDP_ALLOWED: &[&str] = &[
    "Runtime.evaluate",
    "Input.insertText",
    "Input.dispatchKeyEvent",
    "Input.dispatchMouseEvent",
];

/// Finds the first props object on the fiber chain above `sel` that satisfies `pred`; JSON-safe stringify.
const PRELUDE: &str = r#"
const __find = (sel, pred) => {
  for (const el of document.querySelectorAll(sel)) {
    const k = Object.keys(el).find(k => k.startsWith('__reactFiber$'));
    let f = k ? el[k] : null;
    while (f) { const p = f.memoizedProps; if (p && typeof p === 'object' && pred(p)) return p; f = f.return; }
  }
  return null;
};
const __json = (v) => { const seen = new WeakSet(); return JSON.stringify(v, (k, x) => {
  if (typeof x === 'function') return undefined;
  if (x && typeof x === 'object') { if (seen.has(x)) return undefined; seen.add(x); }
  return x; }); };
const __hit = (el, x, y) => { if (!el) return {hit: false, approval: false};
  const h = document.elementFromPoint(x, y); let approval = false;
  for (let a = h; a; a = a.parentElement) { if (el.contains(a)) continue; /* the target itself (found by attribute) */
    const t = ((a.getAttribute && (a.getAttribute('aria-label') || '')) + ' ' + ((a.tagName === 'BUTTON' && a.textContent) || '')).trim();
    if ((a.tagName === 'BUTTON' || (a.getAttribute && a.getAttribute('role') === 'button')) && /^(allow|approve|deny|always allow)/i.test(t)) approval = true;
    if (a.getAttribute && (a.getAttribute('role') === 'alertdialog' || a.hasAttribute('data-approval'))) approval = true; }
  return {hit: !!h && el.contains(h), approval}; };
const __open = () => { const r = document.querySelector('[data-agent-id][data-active="true"]'); return r ? r.getAttribute('data-agent-id') : null; };
"#;

const ROSTER_JS: &str = r#"(() => { const p = __find('[data-agent-id]', p => Array.isArray(p.agents) && ('listRows' in p));
  if (!p) return __json({error: 'sidebar props not found'});
  return __json({agents: p.agents, hiddenAgents: p.hiddenAgents || [], unavailableReason: p.unavailableReason ?? null}); })()"#;

const CHAT_JS: &str = r#"(() => { const p = __find('[data-role]', p => Array.isArray(p.entries) && ('isAgentRunning' in p));
  if (!p) return __json({open: __open(), chat: null});
  return __json({open: __open(), chat: {entries: p.entries, isAgentRunning: p.isAgentRunning, isAgentComposing: p.isAgentComposing,
    agentActivity: p.agentActivity ?? null, loadState: p.loadState ?? null, hasOlder: p.hasOlder ?? null}}); })()"#;

const ROWS_JS: &str = r#"(() => { const out = {}; for (const el of document.querySelectorAll('[data-agent-id]')) {
  const id = el.getAttribute('data-agent-id'); const label = el.getAttribute('aria-label');
  if (!id || (out[id] && !label)) continue;
  const layoutEl = el.closest('[data-layout]');
  out[id] = {id, label: label || (out[id] && out[id].label) || '', desc: el.getAttribute('aria-description') || '',
    awaiting: el.getAttribute('data-awaiting'), composing: el.getAttribute('data-composing'),
    layout: el.getAttribute('data-layout') || (layoutEl ? layoutEl.getAttribute('data-layout') : '')}; }
  return __json(Object.values(out)); })()"#;

const FOCUS_COMPOSER_JS: &str = r#"(() => { const e = document.querySelector('[contenteditable="true"]'); if (!e) return false;
  e.focus(); return document.activeElement === e || e.contains(document.activeElement); })()"#;

pub struct CdpTransport {
    ws: Ws,
    next_id: u64,
    version: String,
    port: u16,
}

fn unavailable(e: impl std::fmt::Display) -> TransportError {
    TransportError::Unavailable(format!("debug port: {e}"))
}

/// The first line of a thrown error (e.g. `edge/invalid-args: listTeamBotAutomations: args.agentId: must be agent
/// id`), scrubbed and cut to 200 characters.
fn exception_text(x: &Value) -> String {
    let d = x
        .pointer("/exception/description")
        .and_then(Value::as_str)
        .or_else(|| x.get("text").and_then(Value::as_str))
        .unwrap_or("error");
    let line: String = d.lines().next().unwrap_or("").chars().take(200).collect();
    floors::scrub(&line)
}

/// Review M2: the point a click may go to, from an in-page hit test `{x, y, hit, approval}`. None: no point (the
/// element was not found). A point whose element is not the target, or that lies on an approval control, is
/// refused (floor 2: botport never answers an approval card).
pub fn click_point(v: &Value) -> TResult<Option<(f64, f64)>> {
    let (Some(x), Some(y)) = (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) else {
        return Ok(None);
    };
    if v.get("approval").and_then(Value::as_bool) != Some(false) {
        return Err(TransportError::Floor(
            "floor 2: the click point is on an approval control; not clicking".into(),
        ));
    }
    if v.get("hit").and_then(Value::as_bool) != Some(true) {
        return Err(TransportError::Floor(
            "floor 2: the element under the click point is not the target (the layout moved); not clicking".into(),
        ));
    }
    Ok(Some((x, y)))
}

/// Review L5: the page target's websocket URL must be `ws://127.0.0.1:<port>` followed by a DevTools page path
/// (so port 9222 cannot match `:92220`, and nothing odd reaches the request line).
pub fn ws_target_path<'a>(url: &'a str, prefix: &str) -> TResult<&'a str> {
    let path = url
        .strip_prefix(prefix)
        .ok_or_else(|| TransportError::Floor("floor 1: target is not on loopback".into()))?;
    if !path.starts_with("/devtools/page/") || !super::ws::valid_ws_path(path) {
        return Err(TransportError::Floor(
            "floor 1: target is not a DevTools page on the port botport opened".into(),
        ));
    }
    Ok(path)
}

fn valid_id(id: &str) -> bool {
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

impl CdpTransport {
    /// Connect through the port file. Checks that the listener is the Grok main process on loopback (floor 1).
    pub fn connect(pf: &PortFile, host: &dyn AppHost) -> TResult<Self> {
        if host.main_pid() != Some(pf.pid) {
            return Err(unavailable(
                "Grok Bot restarted since launch; run botport grok launch",
            ));
        }
        let addr = format!("127.0.0.1:{}", pf.port);
        let ls = host.tcp_listeners(pf.pid);
        if !ls.iter().any(|l| l == &addr) || ls.iter().any(|l| !floors::loopback_only(l)) {
            return Err(TransportError::Floor(
                "floor 1: the debug-port listener is not Grok Bot on 127.0.0.1".into(),
            ));
        }
        let ver = http_get(pf.port, "/json/version").map_err(unavailable)?;
        let ver: Value = serde_json::from_str(&ver).map_err(unavailable)?;
        let ua = ver.get("User-Agent").and_then(Value::as_str).unwrap_or("");
        let version = crate::version::from_user_agent(ua).unwrap_or_else(|| "unknown".into());
        let ws = Self::page_ws(pf.port)?;
        Ok(CdpTransport {
            ws,
            next_id: 0,
            version,
            port: pf.port,
        })
    }

    /// The websocket of the app's one page target on 127.0.0.1:`port` (exact target checks: review L5).
    fn page_ws(port: u16) -> TResult<Ws> {
        let addr = format!("127.0.0.1:{port}");
        let list: Value = serde_json::from_str(&http_get(port, "/json/list").map_err(unavailable)?)
            .map_err(unavailable)?;
        let pages: Vec<&Value> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
            .collect();
        let url = pages
            .first()
            .and_then(|t| t.get("webSocketDebuggerUrl"))
            .and_then(Value::as_str)
            .ok_or_else(|| unavailable("no page target"))?;
        let prefix = format!("ws://{addr}");
        let path = ws_target_path(url, &prefix)?;
        Ws::connect(port, path).map_err(unavailable)
    }

    fn call(&mut self, method: &str, params: Value) -> TResult<Value> {
        if !CDP_ALLOWED.contains(&method) {
            return Err(TransportError::Floor(format!(
                "CDP method {method} is not allowlisted"
            )));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.ws
            .send_text(&json!({"id": id, "method": method, "params": params}).to_string())
            .map_err(unavailable)?;
        loop {
            let m: Value = serde_json::from_str(&self.ws.recv_text().map_err(unavailable)?)
                .map_err(unavailable)?;
            if m.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(e) = m.get("error") {
                    return Err(TransportError::App(format!(
                        "{method}: {}",
                        e.get("message").and_then(Value::as_str).unwrap_or("error")
                    )));
                }
                return Ok(m.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// Evaluate a fixed snippet; the snippet returns a JSON string, parsed here.
    fn eval_json(&mut self, body: &str) -> TResult<Value> {
        let expr = format!("(async () => {{ {PRELUDE}\n return {body}; }})()");
        let r = self.call(
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
        )?;
        if let Some(x) = r.get("exceptionDetails") {
            return Err(TransportError::App(format!(
                "a snippet threw in the app: {}",
                exception_text(x)
            )));
        }
        match r.pointer("/result/value") {
            Some(Value::String(s)) => serde_json::from_str(s)
                .map_err(|e| TransportError::App(format!("snippet JSON: {e}"))),
            Some(v) => Ok(v.clone()),
            None => Ok(Value::Null),
        }
    }

    /// After a launch: wait until the app has rendered its sidebar (see [`super::wait_until_rendered`]).
    pub fn wait_rendered(
        &mut self,
        clock: &crate::clock::SharedClock,
        timeout_ms: u64,
    ) -> TResult<()> {
        super::wait_until_rendered(clock, timeout_ms, || self.roster_raw()).map(|_| ())
    }

    /// The app shows its sign-in screen (no sidebar rows, a sign-in control): issue 02's named FAIL.
    pub fn signed_out(&mut self) -> bool {
        self.eval_json(r#"(() => __json(!document.querySelector('[data-agent-id]') &&
            /\b(sign in|log in|continue with)\b/i.test(document.body ? document.body.innerText : '')))()"#)
            .ok()
            == Some(Value::Bool(true))
    }

    /// The app version seen on connect.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The only way botport reaches `desktop.main` (floor 6).
    pub fn ipc(&mut self, method: &str, args: &Value) -> TResult<Value> {
        if !floors::ipc_allowed(method) {
            return Err(TransportError::Floor(format!(
                "floor 6: app function {method} is not allowlisted"
            )));
        }
        // `Value::Null` args: call with no arguments
        let args = if args.is_null() {
            String::new()
        } else {
            args.to_string()
        };
        let body = format!(
            "(async () => {{ const r = await window.desktop.main[{}]({args}); return __json(r === undefined ? null : r); }})()",
            serde_json::to_string(method).unwrap_or_default(),
        );
        self.eval_json(&body)
    }

    /// Evaluate a snippet that returns a plain string (base64 chunks); no JSON parse.
    fn eval_string(&mut self, body: &str) -> TResult<String> {
        let expr = format!("(async () => {{ {PRELUDE}\n return {body}; }})()");
        let r = self.call(
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
        )?;
        if let Some(x) = r.get("exceptionDetails") {
            return Err(TransportError::App(format!(
                "a snippet threw in the app: {}",
                exception_text(x)
            )));
        }
        r.pointer("/result/value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::App("a snippet returned no string".into()))
    }

    /// Like [`Self::ipc`], with a `Uint8Array` built in the page from chunks staged under `token` set as
    /// `args[field]` (uploads). Same floor-6 check.
    fn ipc_with_bytes(
        &mut self,
        method: &str,
        args: &Value,
        field: &str,
        token: &str,
    ) -> TResult<Value> {
        if !floors::ipc_allowed(method) {
            return Err(TransportError::Floor(format!(
                "floor 6: app function {method} is not allowlisted"
            )));
        }
        let body = format!(
            "(async () => {{ const parts = (window.__botportUp || {{}})[{tok}] || []; delete (window.__botportUp || {{}})[{tok}];
               const n = parts.reduce((a, p) => a + p.length, 0); const bytes = new Uint8Array(n); let o = 0;
               for (const p of parts) {{ bytes.set(p, o); o += p.length; }}
               const a = {args}; a[{field}] = bytes;
               const r = await window.desktop.main[{m}](a); return __json(r === undefined ? null : r); }})()",
            tok = serde_json::to_string(token).unwrap_or_default(),
            field = serde_json::to_string(field).unwrap_or_default(),
            m = serde_json::to_string(method).unwrap_or_default(),
        );
        self.eval_json(&body)
    }

    fn token() -> String {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("t{n:x}")
    }

    /// Read an attachment's bytes in the app (`readAttachmentBytes`), keep them in the page under a token, get the
    /// length and the app's sha256, then pull base64 chunks of [`crate::files::CHUNK`] bytes. The page copy is
    /// dropped at the end, also on error.
    fn read_bytes(&mut self, id: &str, path: &str) -> TResult<(Vec<u8>, Option<String>)> {
        let tok = Self::token();
        let tokj = serde_json::to_string(&tok).unwrap_or_default();
        let mut last = TransportError::App("readAttachmentBytes: no answer".into());
        let mut meta = Value::Null;
        for args in [json!({"path": path}), json!({"agentId": id, "path": path})] {
            if !floors::ipc_allowed("readAttachmentBytes") {
                return Err(TransportError::Floor(
                    "floor 6: app function readAttachmentBytes is not allowlisted".into(),
                ));
            }
            let body = format!(
                r#"(async () => {{ const r = await window.desktop.main["readAttachmentBytes"]({args});
                  let u = null;
                  const view = (x) => x instanceof Uint8Array ? x : x instanceof ArrayBuffer ? new Uint8Array(x)
                    : (x && ArrayBuffer.isView(x)) ? new Uint8Array(x.buffer, x.byteOffset, x.byteLength) : null;
                  u = view(r);
                  if (!u && r && typeof r === 'object') {{ for (const k of ['bytes', 'data', 'buffer', 'content']) {{ u = view(r[k]); if (u) break; }} }}
                  if (!u) return __json({{error: 'shape', type: r === null ? 'null' : typeof r,
                    keys: (r && typeof r === 'object') ? Object.keys(r).slice(0, 20) : null}});
                  (window.__botportDl = window.__botportDl || {{}})[{tokj}] = u;
                  let sha = null;
                  try {{ const d = await crypto.subtle.digest('SHA-256', u);
                    sha = [...new Uint8Array(d)].map(x => x.toString(16).padStart(2, '0')).join(''); }} catch (e) {{}}
                  return __json({{len: u.length, sha}}); }})()"#
            );
            let _ = self.ws.set_read_timeout(Duration::from_secs(180));
            let r = self.eval_json(&body);
            let _ = self.ws.set_read_timeout(Duration::from_secs(15));
            match r {
                Ok(v) if v.get("len").and_then(Value::as_u64).is_some() => {
                    meta = v;
                    break;
                }
                Ok(v) => {
                    last = TransportError::Shape {
                        field: format!(
                            "readAttachmentBytes() answer (type {}, keys {})",
                            v.get("type").and_then(Value::as_str).unwrap_or("?"),
                            v.get("keys").map(Value::to_string).unwrap_or_default()
                        ),
                        version: self.version.clone(),
                    };
                    break;
                }
                // an argument error names the missing field: retry once with the bot id too
                Err(TransportError::App(m)) if m.contains("args") => last = TransportError::App(m),
                Err(e) => return Err(e),
            }
        }
        let Some(len) = meta.get("len").and_then(Value::as_u64) else {
            return Err(last);
        };
        let sha = meta.get("sha").and_then(Value::as_str).map(str::to_string);
        let got = crate::files::assemble(len as usize, |off, n| {
            self.eval_string(&format!(
                "(() => {{ const u = window.__botportDl[{tokj}].subarray({off}, {off} + {n}); let b = '';
                   for (let i = 0; i < u.length; i += 0x8000) b += String.fromCharCode.apply(null, u.subarray(i, i + 0x8000));
                   return btoa(b); }})()"
            ))
            .map_err(|e| e.to_string())
        });
        let _ = self.eval_json(&format!(
            "(() => {{ if (window.__botportDl) delete window.__botportDl[{tokj}]; return __json(true); }})()"
        ));
        let bytes = got.map_err(|e| TransportError::App(format!("download: {e}")))?;
        Ok((bytes, sha))
    }

    /// Keep `u8 expr` (a JS expression giving a Uint8Array) in the page under a token; return (len, sha).
    fn stash_js(tokj: &str, expr: &str) -> String {
        format!(
            r#"const __bp = {expr}; (window.__botportDl = window.__botportDl || {{}})[{tokj}] = __bp; let __sha = null;
              try {{ const d = await crypto.subtle.digest('SHA-256', __bp);
                __sha = [...new Uint8Array(d)].map(x => x.toString(16).padStart(2, '0')).join(''); }} catch (e) {{}}
              return __json({{len: __bp.length, sha: __sha}});"#
        )
    }

    /// Pull the stashed bytes in base64 chunks and drop the page copy.
    fn pull(&mut self, tokj: &str, meta: &Value) -> TResult<(Vec<u8>, Option<String>)> {
        let len = meta
            .get("len")
            .and_then(Value::as_u64)
            .ok_or_else(|| TransportError::App("no byte length".into()))?;
        let sha = meta.get("sha").and_then(Value::as_str).map(str::to_string);
        let got = crate::files::assemble(len as usize, |off, n| {
            self.eval_string(&format!(
                "(() => {{ const u = window.__botportDl[{tokj}].subarray({off}, {off} + {n}); let b = '';
                   for (let i = 0; i < u.length; i += 0x8000) b += String.fromCharCode.apply(null, u.subarray(i, i + 0x8000));
                   return btoa(b); }})()"
            ))
            .map_err(|e| e.to_string())
        });
        let _ = self.eval_json(&format!(
            "(() => {{ if (window.__botportDl) delete window.__botportDl[{tokj}]; return __json(true); }})()"
        ));
        Ok((
            got.map_err(|e| TransportError::App(format!("download: {e}")))?,
            sha,
        ))
    }

    /// The preview route (1.1.0): click the file's "Open <name>" button in its transcript row (the file card's
    /// open control, never an approval card), wait until the preview has loaded the bytes (`{status:"loaded",
    /// bytes:{status:"ready", bytes: Uint8Array}}` in the viewer's state), stash them, close the preview.
    fn preview_bytes(&mut self, key: &str, f: &FileRef) -> TResult<(Vec<u8>, Option<String>)> {
        let keyj = serde_json::to_string(&format!("{key}@")).unwrap_or_default();
        let labelj = serde_json::to_string(&format!("Open {}", f.name)).unwrap_or_default();
        let find = format!(
            r#"(() => {{ const row = [...document.querySelectorAll('[data-row-key]')].find(e => e.getAttribute('data-row-key').startsWith({keyj}));
              if (!row) return __json(null); row.scrollIntoView({{block: 'center'}});
              const b = [...row.querySelectorAll('button[aria-label]')].find(e => e.getAttribute('aria-label') === {labelj});
              if (!b) return __json({{nobutton: true}}); const r = b.getBoundingClientRect();
              const x = r.left + r.width / 2, y = r.top + r.height / 2; return __json({{x, y, ...__hit(b, x, y)}}); }})()"#
        );
        let target = format!(
            r#"(() => {{ const row = [...document.querySelectorAll('[data-row-key]')].find(e => e.getAttribute('data-row-key').startsWith({keyj}));
              return row ? [...row.querySelectorAll('button[aria-label]')].find(e => e.getAttribute('aria-label') === {labelj}) : null; }})()"#
        );
        // the transcript is virtualized: an older row is not in the DOM until it is scrolled to. Step the
        // transcript's own scroller from the top down until the row renders (reading only, no clicks).
        let scroll = r#"(() => { const r = document.querySelector('[data-row-key]'); if (!r) return __json(null);
            let s = r.parentElement; while (s && !(s.scrollHeight > s.clientHeight + 4 && /(auto|scroll)/.test(getComputedStyle(s).overflowY))) s = s.parentElement;
            if (!s) return __json(null); if (!window.__botportScrolled) { window.__botportScrolled = 1; s.scrollTop = 0; return __json(0); }
            const before = s.scrollTop; s.scrollTop = before + Math.max(100, s.clientHeight * 0.7); return __json(s.scrollTop > before ? s.scrollTop : null); })()"#;
        let _ = self.eval_json("(() => { window.__botportScrolled = 0; return __json(true); })()");
        let mut p = self.eval_json(&find)?;
        for _ in 0..400 {
            if p.get("x").is_some() {
                break;
            }
            if self.eval_json(scroll)?.is_null() {
                std::thread::sleep(Duration::from_millis(300));
                p = self.eval_json(&find)?;
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
            p = self.eval_json(&find)?;
        }
        if p.get("x").is_some() {
            // scrollIntoView in `find` may have moved it; read the position again after layout
            std::thread::sleep(Duration::from_millis(300));
            p = self.eval_json(&find)?;
        }
        if p.get("nobutton").is_some() {
            return Err(TransportError::App(format!(
                "the app shows no preview for {} (only Save); ask the bot to send it as .json or .txt",
                f.name
            )));
        }
        let Some((x, y)) = click_point(&p)? else {
            return Err(TransportError::App(format!(
                "the file {} is not on screen in the transcript (message {key})",
                f.name
            )));
        };
        self.checked_click(&target, x, y)?;
        let tok = Self::token();
        let tokj = serde_json::to_string(&tok).unwrap_or_default();
        let grab = format!(
            r#"(async () => {{ const b = document.querySelector('[data-file-viewer-stage-body]'); if (!b) return __json({{wait: 'viewer'}});
              const k = Object.keys(b).find(k => k.startsWith('__reactFiber$')); let f = b[k];
              while (f && !(f.memoizedProps && f.memoizedProps.source && ('downloadName' in f.memoizedProps))) f = f.return;
              if (!f) return __json({{wait: 'viewer state'}});
              let h = f.memoizedState; let m = null;
              while (h) {{ const s = h.memoizedState; if (s && s.status && ('bytes' in s)) {{ m = s; break; }} h = h.next; }}
              if (!m) return __json({{wait: 'load state'}});
              if (m.status === 'error' || (m.bytes && m.bytes.status === 'error')) return __json({{error: 'the preview could not load the file'}});
              let u = m.bytes && m.bytes.status === 'ready' && m.bytes.bytes instanceof Uint8Array ? m.bytes.bytes : null;
              if (!u && typeof m.text === 'string' && m.status === 'loaded') u = new TextEncoder().encode(m.text);
              if (!u) return __json({{wait: 'bytes'}});
              {} }})()"#,
            Self::stash_js(&tokj, "u")
        );
        let mut meta = Value::Null;
        let mut err = None;
        let _ = self.ws.set_read_timeout(Duration::from_secs(60));
        for _ in 0..300 {
            match self.eval_json(&grab) {
                Ok(v) if v.get("len").is_some() => {
                    meta = v;
                    break;
                }
                Ok(v) if v.get("error").is_some() => {
                    err = Some(v["error"].as_str().unwrap_or("error").to_string());
                    break;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(200)),
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        let _ = self.ws.set_read_timeout(Duration::from_secs(15));
        // close the preview (its own "Back to chat" control)
        let close = r#"(() => { const b = document.querySelector('[aria-label="Back to chat"]'); if (b) b.click(); return __json(!!b); })()"#;
        let r = if meta.get("len").is_some() {
            self.pull(&tokj, &meta)
        } else {
            Err(TransportError::App(err.unwrap_or_else(|| {
                "the preview did not load the file within 60 s".into()
            })))
        };
        let _ = self.eval_json(close);
        r
    }

    /// With a pending approval card somewhere in the open chat that the flags do not explain, ask whether one of
    /// the bot's routine runs is open (§3a signal 3); only then is the routine list read.
    fn routine_running(&mut self, id: &str, chat: Option<&parse::Chat>) -> bool {
        let pending = chat.is_some_and(|c| {
            c.messages
                .iter()
                .any(|m| m.kind == MsgKind::Approval && m.answered.is_none())
        });
        pending
            && self
                .ipc("listTeamBotAutomations", &json!({"agentId": id}))
                .is_ok_and(|v| parse::routine_run_open(&v))
    }

    fn roster_raw(&mut self) -> TResult<Value> {
        let v = self.eval_json(ROSTER_JS)?;
        parse::sidebar_props(v, &self.version)
    }

    fn chat_raw(&mut self) -> TResult<(Option<String>, Option<parse::Chat>)> {
        let v = self.eval_json(CHAT_JS)?;
        let open = v.get("open").and_then(Value::as_str).map(str::to_string);
        let chat = match v.get("chat") {
            Some(c) if !c.is_null() => Some(parse::chat(c, &self.version)?),
            _ => None,
        };
        Ok((open, chat))
    }

    /// Wait until the app has replaced our `optimistic:` key with the server key (see [`parse::confirmed_key`]).
    fn confirm_key(&mut self, key: &str) -> TResult<MessageKey> {
        for _ in 0..75 {
            let v = self.eval_json(CHAT_JS)?;
            if let Some(k) = parse::confirmed_key(&v["chat"]["entries"], key) {
                return Ok(k);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(TransportError::App(
            "the app did not confirm the sent message within 15 s".into(),
        ))
    }

    /// Review M2 (floor 2): press at `(x, y)` only if the element under the point is still `target_js` (an
    /// expression for the intended element) or inside it, and not an approval control. Checked again right before
    /// the press, since the chat streams and an approval card can move in between.
    fn checked_click(&mut self, target_js: &str, x: f64, y: f64) -> TResult<()> {
        let recheck = format!("(() => __json(__hit({target_js}, {x}, {y})))()");
        self.call(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y, "button": "left", "clickCount": 1}),
        )?;
        let mut v = self.eval_json(&recheck)?;
        if let Value::Object(m) = &mut v {
            m.insert("x".into(), json!(x));
            m.insert("y".into(), json!(y));
        }
        click_point(&v)?;
        for t in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type": t, "x": x, "y": y, "button": "left", "clickCount": 1}),
            )?;
        }
        Ok(())
    }

    fn click_row(&mut self, id: &str) -> TResult<()> {
        let body = format!(
            r#"(() => {{ const els = [...document.querySelectorAll('[data-agent-id="{id}"]')]; const el = els.find(e => e.getAttribute('aria-label')) || els[0];
              if (!el) return __json(null); el.scrollIntoView({{block: 'center'}}); const r = el.getBoundingClientRect();
              const x = r.left + r.width / 2, y = r.top + r.height / 2; return __json({{x, y, ...__hit(el, x, y)}}); }})()"#
        );
        let target = format!(
            r#"(() => {{ const els = [...document.querySelectorAll('[data-agent-id="{id}"]')]; return els.find(e => e.getAttribute('aria-label')) || els[0]; }})()"#
        );
        let p = self.eval_json(&body)?;
        let Some((x, y)) = click_point(&p)? else {
            return Err(TransportError::App(
                "bot row not found in the sidebar".into(),
            ));
        };
        self.checked_click(&target, x, y)
    }
}

impl Transport for CdpTransport {
    fn name(&self) -> &'static str {
        "debug-port"
    }

    fn probe(&mut self) -> TResult<Capabilities> {
        let caps = [
            Cap::ListBots,
            Cap::Rows,
            Cap::Send,
            Cap::StateFlags,
            Cap::NeedsInput,
            Cap::ReadOpen,
            Cap::Stop,
            Cap::PortCheck,
            Cap::LocalToolPermission,
            Cap::RoutinesList,
            Cap::RoutinesToggle,
            Cap::RoutinesAddViaBot,
            Cap::Usage,
            Cap::Files,
        ]
        .into_iter()
        .collect();
        Ok(Capabilities {
            caps,
            app_version: Some(self.version.clone()),
            tested: crate::version::is_tested(&self.version),
        })
    }

    fn local_tool_permission(&mut self) -> TResult<String> {
        // read only; the setter is not on the floor-6 allowlist
        let v = self.ipc("getLocalToolPermission", &Value::Null)?;
        parse::local_tool_permission(&v).ok_or_else(|| TransportError::Shape {
            field: "getLocalToolPermission()".into(),
            version: self.version.clone(),
        })
    }

    fn port_check(&mut self) -> TResult<String> {
        // the HTTP side still answers, with the same version, and still lists exactly one page target
        let ver: Value =
            serde_json::from_str(&http_get(self.port, "/json/version").map_err(unavailable)?)
                .map_err(unavailable)?;
        let ua = ver.get("User-Agent").and_then(Value::as_str).unwrap_or("");
        let now = crate::version::from_user_agent(ua).unwrap_or_else(|| "unknown".into());
        if now != self.version {
            return Err(unavailable(format!(
                "app version changed from {} to {now} while attached",
                self.version
            )));
        }
        let list: Value =
            serde_json::from_str(&http_get(self.port, "/json/list").map_err(unavailable)?)
                .map_err(unavailable)?;
        let pages = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
            .count();
        if pages != 1 {
            return Err(unavailable(format!("{pages} page targets (expected 1)")));
        }
        // the websocket side still evaluates (a DevTools gate would close or refuse it)
        let r = self.call(
            "Runtime.evaluate",
            json!({"expression": "1 + 1", "returnByValue": true}),
        )?;
        if r.pointer("/result/value").and_then(Value::as_i64) != Some(2) {
            return Err(unavailable("Runtime.evaluate did not answer"));
        }
        Ok(format!(
            "debug port opens; Grok Bot {}; one page target; evaluate answers",
            self.version
        ))
    }

    fn reconnect(&mut self) -> TResult<()> {
        self.ws = Self::page_ws(self.port)?;
        Ok(())
    }

    fn bots(&mut self) -> TResult<Vec<Bot>> {
        let v = self.roster_raw()?;
        let mut bots = parse::roster(&v, &self.version)?;
        let (open, chat) = self.chat_raw()?;
        for b in &mut bots {
            if open.as_deref() == Some(b.id.as_str()) {
                let mut s = parse::snapshot(b, chat.as_ref());
                if s.state != BotState::NeedsInput
                    && self.routine_running(&b.id.clone(), chat.as_ref())
                {
                    s = parse::snapshot_with(b, chat.as_ref(), true);
                }
                b.state = s.state;
                b.activity = s.activity;
                b.needs_input = s.needs_input;
            }
        }
        Ok(bots)
    }

    fn rows(&mut self) -> TResult<Vec<RowView>> {
        let v = self.eval_json(ROWS_JS)?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(parse::row)
            .collect())
    }

    fn bot_state(&mut self, id: &str) -> TResult<BotSnapshot> {
        let v = self.roster_raw()?;
        let bots = parse::roster(&v, &self.version)?;
        let bot = bots
            .into_iter()
            .find(|b| b.id == id)
            .ok_or_else(|| TransportError::App("bot not in the sidebar".into()))?;
        let (open, chat) = self.chat_raw()?;
        let chat = chat.filter(|_| open.as_deref() == Some(id));
        let s = parse::snapshot(&bot, chat.as_ref());
        if s.state != BotState::NeedsInput && self.routine_running(id, chat.as_ref()) {
            return Ok(parse::snapshot_with(&bot, chat.as_ref(), true));
        }
        Ok(s)
    }

    fn open(&mut self, id: &str) -> TResult<()> {
        if !valid_id(id) {
            return Err(TransportError::App("not a bot id".into()));
        }
        if self.chat_raw()?.0.as_deref() == Some(id) {
            return Ok(());
        }
        self.click_row(id)?;
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(200));
            if let (Some(o), Some(_)) = self.chat_raw()? {
                if o == id {
                    return Ok(());
                }
            }
        }
        Err(TransportError::App("the bot's chat did not open".into()))
    }

    fn send(&mut self, id: &str, text: &str, files: &[PathBuf]) -> TResult<MessageKey> {
        if !files.is_empty() {
            return Err(TransportError::Unsupported(Cap::Attach));
        }
        self.open(id)?;
        let before: std::collections::HashSet<String> = self
            .chat_raw()?
            .1
            .map(|c| c.messages.into_iter().map(|m| m.key).collect())
            .unwrap_or_default();
        if self.eval_json(FOCUS_COMPOSER_JS)? != Value::Bool(true) {
            return Err(TransportError::App("the message box was not found".into()));
        }
        self.call("Input.insertText", json!({"text": text}))?;
        for t in ["keyDown", "keyUp"] {
            self.call(
                "Input.dispatchKeyEvent",
                json!({"type": t, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13, "text": if t == "keyDown" { "\r" } else { "" }}),
            )?;
        }
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(200));
            if let (_, Some(c)) = self.chat_raw()? {
                if let Some(m) = c.messages.iter().rev().find(|m| {
                    m.role == Role::User
                        && !before.contains(&m.key)
                        && parse::same_sent_text(&m.text, text)
                }) {
                    return self.confirm_key(&m.key);
                }
            }
        }
        Err(TransportError::App(
            "the sent message did not appear in the transcript".into(),
        ))
    }

    fn routines(&mut self, id: &str) -> TResult<Vec<Routine>> {
        if !valid_id(id) {
            return Err(TransportError::App("not a bot id".into()));
        }
        let v = self.ipc("listTeamBotAutomations", &json!({"agentId": id}))?;
        parse::routines(&v, &self.version)
    }

    fn routine_set_enabled(&mut self, id: &str, r: &str, on: bool) -> TResult<Routine> {
        if !valid_id(id) || !valid_id(r) {
            return Err(TransportError::App("not a bot or routine id".into()));
        }
        let v = self.ipc(
            "setTeamBotAutomationEnabled",
            &json!({"agentId": id, "automationId": r, "isEnabled": on}),
        )?;
        parse::routine_updated(&v, &self.version)
    }

    fn usage(&mut self) -> TResult<Usage> {
        let w = self.ipc("getCursorWeeklyUsage", &json!({}))?;
        let s = self.ipc("getCursorUsageSummary", &json!({}))?;
        parse::usage(&w, &s, &self.version)
    }

    fn files(&mut self, id: &str) -> TResult<Vec<FileEntry>> {
        if !valid_id(id) {
            return Err(TransportError::App("not a bot id".into()));
        }
        let v = self.ipc("listGrokBotAgentFiles", &json!({"agentId": id}))?;
        parse::files(&v, &self.version)
    }

    fn file_attach(&mut self, id: &str, path: &std::path::Path) -> TResult<FileEntry> {
        if !valid_id(id) {
            return Err(TransportError::App("not a bot id".into()));
        }
        // review M4: never follow a symlink swapped in after the engine's check
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(TransportError::Floor(format!(
                "{} is a symlink; not uploaded",
                path.display()
            )));
        }
        let bytes = std::fs::read(path)
            .map_err(|e| TransportError::App(format!("{}: {e}", path.display())))?;
        let name = crate::files::safe_name(
            &path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
        let tok = Self::token();
        let tokj = serde_json::to_string(&tok).unwrap_or_default();
        for c in bytes.chunks(crate::files::CHUNK) {
            let b64 = crate::files::b64_encode(c);
            self.eval_json(&format!(
                "(() => {{ const b = atob({b64:?}); const u = new Uint8Array(b.length);
                   for (let i = 0; i < b.length; i++) u[i] = b.charCodeAt(i);
                   const m = (window.__botportUp = window.__botportUp || {{}}); (m[{tokj}] = m[{tokj}] || []).push(u);
                   return __json(u.length); }})()"
            ))?;
        }
        let args = json!({"agentId": id, "fileName": name, "contentType": crate::files::guess_content_type(path)});
        let _ = self.ws.set_read_timeout(Duration::from_secs(180));
        let r = self.ipc_with_bytes("uploadGrokBotAgentFile", &args, "bytes", &tok);
        let _ = self.ws.set_read_timeout(Duration::from_secs(15));
        parse::uploaded(&r?, &self.version)
    }

    fn file_remove(&mut self, id: &str, name: &str) -> TResult<()> {
        if !valid_id(id) {
            return Err(TransportError::App("not a bot id".into()));
        }
        self.ipc(
            "deleteGrokBotAgentFile",
            &json!({"agentId": id, "name": name}),
        )
        .map(|_| ())
    }

    fn file_read(&mut self, id: &str, f: &FileEntry) -> TResult<Vec<u8>> {
        self.read_bytes(id, &f.path).map(|x| x.0)
    }

    fn file_read_checked(&mut self, id: &str, f: &FileEntry) -> TResult<(Vec<u8>, Option<String>)> {
        if !valid_id(id) {
            return Err(TransportError::App("not a bot id".into()));
        }
        self.read_bytes(id, &f.path)
    }

    fn attachment_read(
        &mut self,
        id: &str,
        key: &str,
        f: &FileRef,
    ) -> TResult<(Vec<u8>, Option<String>)> {
        // the direct call first (a later app version may allow it from the port), then the preview route. The
        // path is bot-written transcript content: only the app's own asset URL shape is read directly (review
        // H2); anything else goes straight to the preview, which shows what the app itself loaded.
        if floors::attachment_url_ok(&f.path, id) {
            match self.read_bytes(id, &f.path) {
                Ok(r) => return Ok(r),
                Err(TransportError::Floor(e)) => return Err(TransportError::Floor(e)),
                Err(_) => {}
            }
        }
        self.open(id)?;
        self.preview_bytes(key, f)
    }

    fn read(&mut self, id: &str, since: Option<&str>, limit: usize) -> TResult<Vec<Message>> {
        let (open, chat) = self.chat_raw()?;
        if open.as_deref() != Some(id) {
            return Err(TransportError::App("the bot's chat is not open".into()));
        }
        let c = chat.ok_or_else(|| TransportError::Shape {
            field: "chat props (entries, isAgentRunning)".into(),
            version: self.version.clone(),
        })?;
        Ok(crate::engine::slice_since(c.messages, since, limit))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids() {
        assert!(super::valid_id("00000000-0000-4000-8000-000000000001"));
        assert!(!super::valid_id("x\"]); alert(1); //00000000-0000-4000"));
    }
    // review M2: a click lands only on its target and never on an approval control
    #[test]
    fn clicks_need_a_hit_on_the_target_and_no_approval_control() {
        use serde_json::json;
        let ok = json!({"x": 10.0, "y": 20.0, "hit": true, "approval": false});
        assert_eq!(super::click_point(&ok).unwrap(), Some((10.0, 20.0)));
        assert_eq!(super::click_point(&json!(null)).unwrap(), None);
        for bad in [
            json!({"x": 10.0, "y": 20.0, "hit": false, "approval": false}),
            json!({"x": 10.0, "y": 20.0, "hit": true, "approval": true}),
            json!({"x": 10.0, "y": 20.0, "hit": false, "approval": true}),
            json!({"x": 10.0, "y": 20.0}),
        ] {
            assert!(
                matches!(
                    super::click_point(&bad),
                    Err(crate::TransportError::Floor(_))
                ),
                "{bad}"
            );
        }
        assert!(super::PRELUDE.contains("document.elementFromPoint(x, y)"));
    }

    // review L5: port 9222 must not match a target on :92220, and the path must be a DevTools page
    #[test]
    fn the_target_url_must_be_exactly_our_port_and_a_page_path() {
        let pre = "ws://127.0.0.1:9222";
        assert_eq!(
            super::ws_target_path("ws://127.0.0.1:9222/devtools/page/SYNTHETIC1", pre).unwrap(),
            "/devtools/page/SYNTHETIC1"
        );
        for u in [
            "ws://127.0.0.1:92220/devtools/page/SYNTHETIC1",
            "ws://127.0.0.1:9222/devtools/page/a\r\nX: y",
            "ws://127.0.0.1:9222/devtools/browser/SYNTHETIC1",
            "ws://10.0.0.1:9222/devtools/page/SYNTHETIC1",
        ] {
            assert!(super::ws_target_path(u, pre).is_err(), "{u:?}");
        }
    }

    #[test]
    fn cdp_allowlist_is_narrow() {
        for m in ["Runtime.evaluate", "Input.insertText"] {
            assert!(super::CDP_ALLOWED.contains(&m));
        }
        for m in [
            "Page.navigate",
            "Network.getCookies",
            "Runtime.callFunctionOn",
            "Browser.close",
            "Storage.getCookies",
        ] {
            assert!(!super::CDP_ALLOWED.contains(&m));
        }
    }
}

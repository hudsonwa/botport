//! A [`Store`] over the Google Drive API v3, through the `curl` command (no HTTP crate in botport).
//!
//! - The access token comes from a command the owner configures, run as an argv (no shell, a 30 s limit) with its
//!   stderr discarded; it must match `[A-Za-z0-9._~+/=-]+`. It reaches `curl` only as a config line on curl's stdin
//!   (`-K -`), so it is never in an argv, the environment, a file, a log or an error. `-q` keeps a user `.curlrc`
//!   (which could turn on tracing) out of it; an https base is called with `--proto =https`.
//! - Only files inside the configured folder are listed or touched; paths map to subfolders by name.
//! - Files whose last modifying user is not the owner (`lastModifyingUser.me` false) are ignored.
//! - Every call has a time limit (`curl --max-time`); errors name the operation and HTTP status, never a response body
//!   (it can hold file names).
use super::store::{Entry, Store};
use super::IoStats;
use crate::core::util::{err, safe_rel, write_private, Res};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// The largest file downloaded.
pub const MAX_DOWNLOAD: u64 = 64 << 20;
/// At most this many result pages per listing.
const MAX_PAGES: usize = 50;
/// The token command's time limit.
const TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How to reach the folder.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// The Drive file id of the pair's folder (`botport-mirror/<pair>`).
    pub folder_id: String,
    /// The command that prints an access token (argv, no shell).
    pub token_cmd: Vec<String>,
    /// API base: always `https://www.googleapis.com` (only this module's tests point it at a local stub).
    base: String,
    /// Per-call time limit, seconds.
    pub timeout_s: u64,
    /// The `curl` binary.
    pub curl: String,
    /// Where downloaded files are kept between runs (0700; files 0600), so a run where nothing changed makes one
    /// listing call per folder level and no download.
    pub cache_dir: Option<PathBuf>,
}

impl ApiConfig {
    /// Defaults for a folder id and token command.
    pub fn new(folder_id: &str, token_cmd: Vec<String>) -> ApiConfig {
        ApiConfig {
            folder_id: folder_id.into(),
            token_cmd,
            base: "https://www.googleapis.com".into(),
            timeout_s: 30,
            curl: "curl".into(),
            cache_dir: None,
        }
    }
}

/// The Drive API store.
pub struct ApiStore {
    cfg: ApiConfig,
    token: Option<String>,
    /// directory path -> folder id (the root is "")
    dirs: BTreeMap<String, String>,
    /// every file and folder under the root, from one listing per run (built on first use)
    nodes: Option<Vec<Node>>,
    io: IoStats,
}

/// One file or folder of the listing.
#[derive(Debug, Clone)]
struct Node {
    id: String,
    name: String,
    parent: String,
    size: u64,
    md5: Option<String>,
    folder: bool,
}

/// A token must be plain (no quotes, spaces or newlines that could change the curl config).
pub fn valid_token(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 4096
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~+/=-".contains(&b))
}

/// Percent-encoding for a URL query value.
pub fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A Drive query string literal: `'` and `\` escaped.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The arguments for one curl call: never the token (that is in the config file).
pub fn curl_args(
    cfg: &ApiConfig,
    method: &str,
    url: &str,
    body: Option<(&Path, &str)>,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "-q",
        "-sS",
        "--max-time",
        &cfg.timeout_s.to_string(),
        "--max-filesize",
        &MAX_DOWNLOAD.to_string(),
        "-K",
        "-",
        "-X",
        method,
        "-H",
        "Expect:",
        "-w",
        "\n%{http_code}",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if cfg.base.starts_with("https://") {
        a.extend(["--proto".to_string(), "=https".to_string()]);
    }
    if let Some((p, ctype)) = body {
        a.extend([
            "-H".to_string(),
            format!("Content-Type: {ctype}"),
            "--data-binary".to_string(),
            format!("@{}", p.display()),
        ]);
    }
    a.push(url.to_string());
    a
}

/// A private temp dir that removes itself.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new() -> Res<TmpDir> {
        for n in 0..100u32 {
            let d = std::env::temp_dir().join(format!(
                "botport-curl-{}-{}",
                std::process::id(),
                super::pack::random_id() + &n.to_string()
            ));
            let mut b = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            if b.create(&d).is_ok() {
                return Ok(TmpDir(d));
            }
        }
        err("no private temp dir for curl")
    }

    fn file(&self, name: &str, data: &[u8]) -> Res<PathBuf> {
        let p = self.0.join(name);
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&p).map_err(|e| format!("temp file: {e}"))?;
        f.write_all(data).map_err(|e| format!("temp file: {e}"))?;
        Ok(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        if let Ok(rd) = fs::read_dir(&self.0) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

impl ApiStore {
    /// A store for one folder; the token is fetched on first use.
    pub fn new(cfg: ApiConfig) -> ApiStore {
        // folder ids come only from this run's walk down from the configured folder (`index`)
        let mut dirs: BTreeMap<String, String> = BTreeMap::new();
        dirs.insert(String::new(), cfg.folder_id.clone());
        ApiStore {
            cfg,
            token: None,
            dirs,
            nodes: None,
            io: IoStats::default(),
        }
    }

    fn token(&mut self) -> Res<String> {
        if let Some(t) = &self.token {
            return Ok(t.clone());
        }
        let (prog, args) = self
            .cfg
            .token_cmd
            .split_first()
            .ok_or("no token command configured")?;
        let mut child = Command::new(crate::core::util::program(prog)?)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("token command could not run: {}", e.kind()))?;
        let mut stdout = child.stdout.take().ok_or("token command has no output")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut s = Vec::new();
            std::io::Read::read_to_end(&mut stdout, &mut s).ok();
            tx.send(s).ok();
        });
        let t0 = Instant::now();
        let status = loop {
            if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
                break st;
            }
            if t0.elapsed() > TOKEN_TIMEOUT {
                child.kill().ok();
                child.wait().ok();
                return err("token command timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // a grandchild still holding the pipe must not hang the run
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "token command left its output open".to_string())?;
        if !status.success() {
            return err("token command failed");
        }
        let t = String::from_utf8(out)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !valid_token(&t) {
            return err("token command printed something that is not a plain access token");
        }
        self.token = Some(t.clone());
        Ok(t)
    }

    /// One call: (HTTP status, body).
    fn call(
        &mut self,
        what: &str,
        method: &str,
        url: &str,
        body: Option<(&[u8], &str)>,
    ) -> Res<(u16, Vec<u8>)> {
        self.call_capped(what, method, url, body, None)
    }

    /// [`Self::call`]; with `cap`, the request asks for at most that many bytes (a Range) and the response body goes
    /// to a private file of which at most `cap` bytes are read.
    fn call_capped(
        &mut self,
        what: &str,
        method: &str,
        url: &str,
        body: Option<(&[u8], &str)>,
        cap: Option<u64>,
    ) -> Res<(u16, Vec<u8>)> {
        if let Some(rest) = self.cfg.base.strip_prefix("http://") {
            if !local_stub(rest) {
                return err("Drive API: a plain http base is allowed only for a local stub");
            }
        }
        let token = self.token()?;
        let tmp = TmpDir::new()?;
        let bp = match body {
            Some((b, _)) => Some(tmp.file("b", b)?),
            None => None,
        };
        let mut args = curl_args(
            &self.cfg,
            method,
            url,
            bp.as_deref().zip(body.map(|(_, c)| c)),
        );
        let capped_out = match cap {
            Some(c) => {
                let p = tmp.file("o", b"")?;
                // before the url (the last argument): ask for bytes 0..c-1 only, and write the body to the file
                let at = args.len() - 1;
                args.splice(
                    at..at,
                    [
                        "-r".to_string(),
                        format!("0-{}", c.saturating_sub(1)),
                        "-o".to_string(),
                        p.display().to_string(),
                    ],
                );
                Some((p, c))
            }
            None => None,
        };
        let t0 = Instant::now();
        let out = (|| -> Res<std::process::Output> {
            let mut child = Command::new(crate::core::util::tool(&self.cfg.curl)?)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("Drive API {what}: curl could not run: {}", e.kind()))?;
            // the token travels on curl's stdin only, as a config line, and the pipe is closed right after
            let mut stdin = child.stdin.take().ok_or("curl has no stdin")?;
            stdin
                .write_all(format!("header = \"Authorization: Bearer {token}\"\n").as_bytes())
                .map_err(|_| format!("Drive API {what}: curl did not take its config"))?;
            drop(stdin);
            child
                .wait_with_output()
                .map_err(|e| format!("Drive API {what}: {}", e.kind()))
        })();
        let capped_body = match &capped_out {
            Some((p, c)) => {
                let mut b = Vec::new();
                let f = fs::File::open(p).map_err(|e| format!("Drive API {what}: {}", e.kind()))?;
                std::io::Read::read_to_end(&mut std::io::Read::take(f, *c), &mut b)
                    .map_err(|e| format!("Drive API {what}: {}", e.kind()))?;
                Some(b)
            }
            None => None,
        };
        drop(tmp);
        let out = out?;
        self.io.net_ms += t0.elapsed().as_millis() as u64;
        if let Some((b, _)) = body {
            self.io.written_bytes += b.len() as u64;
        }
        let mut b = out.stdout;
        let nl = b
            .iter()
            .rposition(|&c| c == b'\n')
            .ok_or_else(|| format!("Drive API {what}: no response"))?;
        let code: u16 = std::str::from_utf8(&b[nl + 1..])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| {
                format!("Drive API {what}: no HTTP status (network error or timeout)")
            })?;
        b.truncate(nl);
        if let Some(cb) = capped_body {
            b = cb;
        }
        // a Range past the end of a file that emptied since the listing: nothing to read
        if cap.is_some() && code == 416 {
            return Ok((code, vec![]));
        }
        self.io.read_bytes += b.len() as u64;
        if !(200..300).contains(&code) {
            return err(format!("Drive API {what}: HTTP {code}"));
        }
        Ok((code, b))
    }

    fn json(
        &mut self,
        what: &str,
        method: &str,
        url: &str,
        body: Option<(&[u8], &str)>,
    ) -> Res<Value> {
        let (_, b) = self.call(what, method, url, body)?;
        if b.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&b).map_err(|_| format!("Drive API {what}: response is not JSON"))
    }

    /// One listing of every child of the given folders (only files the owner last modified).
    fn list_many(&mut self, parents: &[String]) -> Res<Vec<Node>> {
        let ors: Vec<String> = parents
            .iter()
            .map(|p| format!("{} in parents", lit(p)))
            .collect();
        let q = format!("trashed = false and ({})", ors.join(" or "));
        let mut out = vec![];
        let mut page = String::new();
        let mut pages = 0usize;
        loop {
            let mut url = format!(
                "{}/drive/v3/files?q={}&fields={}&pageSize=1000",
                self.cfg.base,
                pct(&q),
                pct("nextPageToken,files(id,name,size,md5Checksum,parents,mimeType,lastModifyingUser(me))")
            );
            if !page.is_empty() {
                url.push_str(&format!("&pageToken={}", pct(&page)));
            }
            let v = self.json("list", "GET", &url, None)?;
            for f in v["files"].as_array().into_iter().flatten() {
                // only files the owner last modified (a missing field counts as someone else)
                if f["lastModifyingUser"]["me"] != true {
                    continue;
                }
                let (Some(name), Some(id), Some(parent)) = (
                    f["name"].as_str(),
                    f["id"].as_str(),
                    f["parents"][0].as_str(),
                ) else {
                    continue;
                };
                if name.contains('/') || name.is_empty() || !plain_id(id) {
                    continue;
                }
                out.push(Node {
                    id: id.into(),
                    name: name.into(),
                    parent: parent.into(),
                    size: f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                    md5: f["md5Checksum"]
                        .as_str()
                        .filter(|m| plain_md5(m))
                        .map(String::from),
                    folder: f["mimeType"] == FOLDER_MIME,
                });
            }
            pages += 1;
            match v["nextPageToken"].as_str() {
                Some(t) if !t.is_empty() && pages < MAX_PAGES => page = t.to_string(),
                Some(t) if !t.is_empty() => {
                    return err("Drive API list: too many files in a mirror folder")
                }
                _ => break,
            }
        }
        Ok(out)
    }

    /// The listing of the whole tree, from the configured folder down: one call per folder level.
    fn index(&mut self) -> Res<&Vec<Node>> {
        if self.nodes.is_none() {
            let root = self.cfg.folder_id.clone();
            let mut nodes: Vec<Node> = vec![];
            // listed from the configured folder down, one call per level: only the pair folder and folders reached from
            // it in this run are ever listed (a remembered id is never listed before this run reaches it)
            let mut to_list: Vec<String> = vec![root.clone()];
            let mut listed: std::collections::BTreeSet<String> = Default::default();
            while !to_list.is_empty() {
                listed.extend(to_list.iter().cloned());
                let got = self.list_many(&to_list)?;
                to_list = got
                    .iter()
                    .filter(|n| n.folder && !listed.contains(&n.id))
                    .map(|n| n.id.clone())
                    .collect();
                nodes.extend(got);
            }
            // paths come only from a walk down from the pair folder: a remembered folder that was moved elsewhere
            // (or ids from another pair) is never used, and nothing outside the pair folder counts
            let mut dirs: BTreeMap<String, String> = BTreeMap::new();
            dirs.insert(String::new(), root.clone());
            let mut frontier = vec![(String::new(), root.clone())];
            while let Some((path, id)) = frontier.pop() {
                let mut seen_names: std::collections::BTreeSet<&str> = Default::default();
                for n in nodes.iter().filter(|n| n.folder && n.parent == id) {
                    if !seen_names.insert(&n.name) {
                        return err("Drive API: two folders with the same name in a mirror folder; remove one");
                    }
                    let child = if path.is_empty() {
                        n.name.clone()
                    } else {
                        format!("{path}/{}", n.name)
                    };
                    dirs.insert(child.clone(), n.id.clone());
                    frontier.push((child, n.id.clone()));
                }
            }
            let reachable: std::collections::BTreeSet<String> = dirs.values().cloned().collect();
            nodes.retain(|n| reachable.contains(&n.parent));
            self.dirs = dirs;
            self.prune_cache(&nodes);
            self.nodes = Some(nodes);
        }
        Ok(self.nodes.as_ref().expect("built above"))
    }

    /// Drop cached files no listed file refers to any more.
    fn prune_cache(&self, nodes: &[Node]) {
        let Some(d) = &self.cfg.cache_dir else { return };
        let live: std::collections::BTreeSet<&str> =
            nodes.iter().filter_map(|n| n.md5.as_deref()).collect();
        if let Ok(rd) = fs::read_dir(d.join("files")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if plain_md5(&n) && !live.contains(n.as_str()) {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }

    /// The one node named `name` in folder `dir` (`folder`: a folder or a file); two of them is an error.
    fn node(&mut self, dir: &str, name: &str, folder: bool) -> Res<Option<Node>> {
        // the walk first: a folder id is used only once this run has reached it from the configured folder
        self.index()?;
        let Some(pid) = self.dirs.get(dir).cloned() else {
            return Ok(None);
        };
        let mut m = self
            .index()?
            .iter()
            .filter(|n| n.parent == pid && n.name == name && n.folder == folder);
        let first = m.next().cloned();
        if m.next().is_some() {
            return err("Drive API: two files with the same name in a mirror folder; remove one");
        }
        Ok(first)
    }

    /// The folder id for `dir`, creating missing folders.
    fn make_dir(&mut self, dir: &str) -> Res<String> {
        self.index()?;
        if let Some(id) = self.dirs.get(dir) {
            return Ok(id.clone());
        }
        let (parent, name) = dir.rsplit_once('/').unwrap_or(("", dir));
        let pid = self.make_dir(parent)?;
        let meta = json!({"name": name, "mimeType": FOLDER_MIME, "parents": [pid]}).to_string();
        let url = format!("{}/drive/v3/files?fields=id", self.cfg.base);
        let v = self.json(
            "mkdir",
            "POST",
            &url,
            Some((meta.as_bytes(), "application/json")),
        )?;
        let id = v["id"]
            .as_str()
            .filter(|i| plain_id(i))
            .ok_or("Drive API mkdir: no id")?
            .to_string();
        self.dirs.insert(dir.to_string(), id.clone());
        if let Some(nodes) = self.nodes.as_mut() {
            nodes.push(Node {
                id: id.clone(),
                name: name.into(),
                parent: pid,
                size: 0,
                md5: None,
                folder: true,
            });
        }
        Ok(id)
    }

    fn cached(&self, md5: Option<&str>) -> Option<Vec<u8>> {
        let (d, m) = (self.cfg.cache_dir.as_ref()?, md5?);
        let root = d.canonicalize().ok()?;
        crate::core::util::read_confined(&root, &root.join("files"), m).ok()
    }

    fn cache(&self, md5: Option<&str>, data: &[u8]) {
        if let (Some(d), Some(m)) = (&self.cfg.cache_dir, md5) {
            let _ = write_private(d, &format!("files/{m}"), data);
        }
    }

    /// Record an upload's answer (`id`, `md5Checksum`, `size`) in the index and the cache.
    fn uploaded(&mut self, v: &Value, dir_id: &str, name: &str, data: &[u8]) {
        let Some(id) = v["id"].as_str().filter(|i| plain_id(i)) else {
            return;
        };
        let md5 = v["md5Checksum"]
            .as_str()
            .filter(|m| plain_md5(m))
            .map(String::from);
        self.cache(md5.as_deref(), data);
        if let Some(nodes) = self.nodes.as_mut() {
            nodes.retain(|n| n.id != id);
            nodes.push(Node {
                id: id.into(),
                name: name.into(),
                parent: dir_id.into(),
                size: data.len() as u64,
                md5,
                folder: false,
            });
        }
    }
}

/// A Drive id as botport accepts one (it goes into urls and file names).
/// Whether the authority of a plain-http base (what follows `http://`) is a local stub: exactly `127.0.0.1`,
/// `localhost` or `[::1]`, with an optional numeric port and nothing else (no userinfo, no other host).
fn local_stub(rest: &str) -> bool {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let Some((h, after)) = v6.split_once(']') else {
            return false;
        };
        (format!("[{h}]"), after)
    } else {
        match authority.find(':') {
            Some(i) => (authority[..i].to_string(), &authority[i..]),
            None => (authority.to_string(), ""),
        }
    };
    let port_ok = port.is_empty()
        || port.strip_prefix(':').is_some_and(|n| {
            !n.is_empty() && n.len() <= 5 && n.bytes().all(|b| b.is_ascii_digit())
        });
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") && port_ok
}

fn plain_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A Drive md5 checksum: 32 lowercase hex characters (it names a cache file).
fn plain_md5(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Store for ApiStore {
    fn list(&mut self, dir: &str) -> Res<Vec<Entry>> {
        if !dir.is_empty() {
            safe_rel("mirror folder", dir)?;
        }
        self.index()?;
        let Some(id) = self.dirs.get(dir).cloned() else {
            return Ok(vec![]);
        };
        let mut out: Vec<Entry> = self
            .index()?
            .iter()
            .filter(|n| n.parent == id && !n.folder)
            .map(|n| Entry {
                name: n.name.clone(),
                size: n.size,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn get(&mut self, path: &str) -> Res<Option<Vec<u8>>> {
        safe_rel("mirror file", path)?;
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let Some(n) = self.node(dir, name, false)? else {
            return Ok(None);
        };
        // unchanged since a run downloaded it: served from the cache, no call
        if let Some(b) = self.cached(n.md5.as_deref()) {
            return Ok(Some(b));
        }
        let url = format!("{}/drive/v3/files/{}?alt=media", self.cfg.base, pct(&n.id));
        let b = self.call("download", "GET", &url, None)?.1;
        // cached under its checksum only when the file did not change between the listing and the download
        if n.md5.is_some() {
            let murl = format!(
                "{}/drive/v3/files/{}?fields=md5Checksum",
                self.cfg.base,
                pct(&n.id)
            );
            if self.json("checksum", "GET", &murl, None)?["md5Checksum"].as_str()
                == n.md5.as_deref()
            {
                self.cache(n.md5.as_deref(), &b);
            }
        }
        Ok(Some(b))
    }

    fn get_at_most(&mut self, path: &str, max: u64) -> Res<Option<Vec<u8>>> {
        safe_rel("mirror file", path)?;
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let Some(n) = self.node(dir, name, false)? else {
            return Ok(None);
        };
        // an empty file (an answer created but not filled in yet) needs no download: a Range on it is unsatisfiable
        if n.size == 0 {
            return Ok(Some(vec![]));
        }
        let url = format!("{}/drive/v3/files/{}?alt=media", self.cfg.base, pct(&n.id));
        let cap = max.saturating_add(1);
        let b = self
            .call_capped("download", "GET", &url, None, Some(cap))?
            .1;
        Ok(Some(b))
    }

    fn put(&mut self, path: &str, data: &[u8]) -> Res<()> {
        safe_rel("mirror file", path)?;
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let pid = self.make_dir(dir)?;
        let existing = self.node(dir, name, false)?;
        let v = match existing {
            Some(n) => {
                let url = format!(
                    "{}/upload/drive/v3/files/{}?uploadType=media&fields=id,md5Checksum,size",
                    self.cfg.base,
                    pct(&n.id)
                );
                self.json(
                    "update",
                    "PATCH",
                    &url,
                    Some((data, "application/octet-stream")),
                )?
            }
            None => {
                let boundary = format!("botport-{}", super::pack::random_id());
                let meta = json!({"name": name, "parents": [pid]}).to_string();
                let mut body = format!(
                    "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .into_bytes();
                body.extend_from_slice(data);
                body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
                let url = format!(
                    "{}/upload/drive/v3/files?uploadType=multipart&fields=id,md5Checksum,size",
                    self.cfg.base
                );
                let ctype = format!("multipart/related; boundary={boundary}");
                self.json("create", "POST", &url, Some((&body, &ctype)))?
            }
        };
        self.uploaded(&v, &pid, name, data);
        Ok(())
    }

    fn delete(&mut self, path: &str) -> Res<()> {
        safe_rel("mirror file", path)?;
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        if let Some(n) = self.node(dir, name, false)? {
            let url = format!("{}/drive/v3/files/{}", self.cfg.base, pct(&n.id));
            self.call("delete", "DELETE", &url, None)?;
            if let Some(nodes) = self.nodes.as_mut() {
                nodes.retain(|x| x.id != n.id);
            }
        }
        Ok(())
    }

    fn io(&self) -> IoStats {
        self.io
    }

    fn refresh(&mut self) {
        self.nodes = None;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_local_stub_may_use_plain_http() {
        for ok in [
            "127.0.0.1:8080",
            "localhost",
            "localhost:1/x",
            "[::1]:9/drive",
        ] {
            assert!(local_stub(ok), "{ok}");
        }
        for bad in [
            "localhost:1@evil.example",
            "user@127.0.0.1",
            "localhost.evil.example",
            "127.0.0.1:80:90",
            "[::1",
            "127.0.0.2",
            "evil.example",
        ] {
            assert!(!local_stub(bad), "{bad}");
        }
    }

    use super::*;
    use crate::core::util::sha256_hex;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "ya29.example-token_0123456789";

    #[derive(Default)]
    struct Fake {
        /// id -> (name, parent, folder, data, me)
        files: BTreeMap<String, (String, String, bool, Vec<u8>, bool)>,
        next: u32,
        auth_ok: bool,
        calls: Vec<String>,
    }

    fn query_param(url: &str, k: &str) -> Option<String> {
        let q = url.split_once('?')?.1;
        q.split('&').find_map(|kv| {
            let (a, b) = kv.split_once('=')?;
            (a == k).then(|| {
                let mut out = vec![];
                let bs = b.as_bytes();
                let mut i = 0;
                while i < bs.len() {
                    if bs[i] == b'%' {
                        out.push(u8::from_str_radix(&b[i + 1..i + 3], 16).unwrap());
                        i += 3;
                    } else {
                        out.push(bs[i]);
                        i += 1;
                    }
                }
                String::from_utf8(out).unwrap()
            })
        })
    }

    /// A small stand-in for the Drive API: enough of files.list/get/create/update/delete for the store.
    fn serve(fake: Arc<Mutex<Fake>>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let (method, url) = (
                    parts.next().unwrap().to_string(),
                    parts.next().unwrap().to_string(),
                );
                let (mut len, mut auth) = (0usize, String::new());
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    match k.to_ascii_lowercase().as_str() {
                        "content-length" => len = v.trim().parse().unwrap(),
                        "authorization" => auth = v.trim().to_string(),
                        _ => {}
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let mut f = fake.lock().unwrap();
                f.auth_ok = auth == format!("Bearer {TOKEN}");
                f.calls
                    .push(format!("{method} {}", url.split('?').next().unwrap()));
                let path = url.split('?').next().unwrap().to_string();
                let (code, resp): (u16, Vec<u8>) = if !f.auth_ok {
                    (401, b"{}".to_vec())
                } else if method == "GET" && path == "/drive/v3/files" {
                    let q = query_param(&url, "q").unwrap();
                    // every 'id' in parents clause of the query
                    let parents: Vec<String> = q
                        .split('\'')
                        .collect::<Vec<_>>()
                        .chunks(2)
                        .filter_map(|c| c.get(1).map(|s| s.to_string()))
                        .collect();
                    let files: Vec<Value> = f
                        .files
                        .iter()
                        .filter(|(_, v)| parents.contains(&v.1))
                        .map(|(id, v)| {
                            json!({"id": id, "name": v.0, "size": v.3.len().to_string(),
                                   "md5Checksum": &sha256_hex(&v.3)[..32], "parents": [v.1],
                                   "mimeType": if v.2 { FOLDER_MIME } else { "application/octet-stream" },
                                   "lastModifyingUser": {"me": v.4}})
                        })
                        .collect();
                    (200, json!({"files": files}).to_string().into_bytes())
                } else if method == "GET" && path.starts_with("/drive/v3/files/") {
                    let id = &path["/drive/v3/files/".len()..];
                    match f.files.get(id) {
                        Some(v) if url.contains("fields=md5Checksum") => (
                            200,
                            json!({"md5Checksum": &sha256_hex(&v.3)[..32]})
                                .to_string()
                                .into_bytes(),
                        ),
                        Some(v) => (200, v.3.clone()),
                        None => (404, b"{}".to_vec()),
                    }
                } else if method == "POST" && path == "/drive/v3/files" {
                    let m: Value = serde_json::from_slice(&body).unwrap();
                    f.next += 1;
                    let id = format!("d{}", f.next);
                    let parent = m["parents"][0].as_str().unwrap().to_string();
                    f.files.insert(
                        id.clone(),
                        (
                            m["name"].as_str().unwrap().into(),
                            parent,
                            true,
                            vec![],
                            true,
                        ),
                    );
                    (200, json!({"id": id}).to_string().into_bytes())
                } else if method == "POST" && path == "/upload/drive/v3/files" {
                    let t = body.clone();
                    let s = String::from_utf8_lossy(&t).to_string();
                    let boundary = s
                        .lines()
                        .next()
                        .unwrap()
                        .trim_start_matches("--")
                        .to_string();
                    let parts: Vec<&str> = s.split(&format!("--{boundary}")).collect();
                    let meta: Value =
                        serde_json::from_str(parts[1].split("\r\n\r\n").nth(1).unwrap().trim())
                            .unwrap();
                    let head = format!("--{boundary}");
                    let start = t
                        .windows(4)
                        .enumerate()
                        .filter(|(_, w)| *w == b"\r\n\r\n")
                        .nth(1)
                        .unwrap()
                        .0
                        + 4;
                    let end = t.len() - (head.len() + 6);
                    f.next += 1;
                    let id = format!("f{}", f.next);
                    let parent = meta["parents"][0].as_str().unwrap().to_string();
                    let data = t[start..end].to_vec();
                    let md5 = sha256_hex(&data)[..32].to_string();
                    f.files.insert(
                        id.clone(),
                        (
                            meta["name"].as_str().unwrap().into(),
                            parent,
                            false,
                            data,
                            true,
                        ),
                    );
                    (
                        200,
                        json!({"id": id, "md5Checksum": md5})
                            .to_string()
                            .into_bytes(),
                    )
                } else if method == "PATCH" && path.starts_with("/upload/drive/v3/files/") {
                    let id = path["/upload/drive/v3/files/".len()..].to_string();
                    f.files.get_mut(&id).unwrap().3 = body.clone();
                    (
                        200,
                        json!({"id": id, "md5Checksum": &sha256_hex(&body)[..32]})
                            .to_string()
                            .into_bytes(),
                    )
                } else if method == "DELETE" {
                    let id = &path["/drive/v3/files/".len()..];
                    f.files.remove(id);
                    (204, vec![])
                } else {
                    (400, b"{}".to_vec())
                };
                drop(f);
                let mut s = s;
                let _ = write!(
                    s,
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.len()
                );
                let _ = s.write_all(&resp);
            }
        });
        format!("http://{addr}")
    }

    fn store(base: &str, token_cmd: Vec<String>) -> ApiStore {
        let mut cfg = ApiConfig::new("root", token_cmd);
        cfg.base = base.into();
        cfg.timeout_s = 10;
        ApiStore::new(cfg)
    }

    fn echo_token() -> Vec<String> {
        vec!["echo".into(), TOKEN.into()]
    }

    #[test]
    fn a_bounded_download_keeps_at_most_max_plus_one_bytes() {
        if Command::new("curl").arg("--version").output().is_err() {
            eprintln!("curl not found; skipping");
            return;
        }
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone());
        let mut s = store(&base, echo_token());
        s.put("probe/bot.txt", &vec![b'x'; 10_000]).unwrap();
        // the stand-in ignores the Range the request carries: at most max + 1 bytes are read all the same
        let got =
            |s: &mut ApiStore, max| s.get_at_most("probe/bot.txt", max).unwrap().unwrap().len();
        assert_eq!(got(&mut s, 4096), 4097);
        assert_eq!(got(&mut s, 20_000), 10_000);
        assert_eq!(s.get_at_most("probe/none.txt", 4096).unwrap(), None);
        // an empty answer file is read as empty, with no download at all
        s.put("probe/r1/ack-1.txt", b"").unwrap();
        let downloads = |f: &Arc<Mutex<Fake>>| {
            f.lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| c.starts_with("GET /drive/v3/files/"))
                .count()
        };
        let before = downloads(&fake);
        assert_eq!(
            s.get_at_most("probe/r1/ack-1.txt", 4096).unwrap(),
            Some(vec![])
        );
        assert_eq!(downloads(&fake), before);
    }

    #[test]
    fn round_trip_through_the_api() {
        if Command::new("curl").arg("--version").output().is_err() {
            eprintln!("curl not found; skipping");
            return;
        }
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone());
        let mut s = store(&base, echo_token());
        assert_eq!(s.list("from-grok").unwrap(), vec![]);
        assert_eq!(s.get("HEAD").unwrap(), None);
        s.put("HEAD", b"gen=1 pack=p sha=s machine=m\n").unwrap();
        s.put("to-grok/blobs/abc", &[0, 1, 2, 255]).unwrap();
        s.put("HEAD", b"gen=2 pack=p sha=s machine=m\n").unwrap();
        assert!(fake.lock().unwrap().auth_ok);
        assert_eq!(
            s.get("HEAD").unwrap().unwrap(),
            b"gen=2 pack=p sha=s machine=m\n"
        );
        assert_eq!(
            s.get("to-grok/blobs/abc").unwrap().unwrap(),
            vec![0, 1, 2, 255]
        );
        let names: Vec<String> = s
            .list("to-grok")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.is_empty(), "folders are not files");
        assert_eq!(s.list("to-grok/blobs").unwrap()[0].size, 4);
        s.delete("to-grok/blobs/abc").unwrap();
        assert_eq!(s.get("to-grok/blobs/abc").unwrap(), None);
        // one folder per directory, created once
        let folders = fake.lock().unwrap().files.values().filter(|v| v.2).count();
        assert_eq!(folders, 2);
        assert!(s.io().net_ms > 0 || s.io().read_bytes > 0);
    }

    #[test]
    fn a_second_run_with_nothing_changed_makes_one_listing_per_level() {
        if Command::new("curl").arg("--version").output().is_err() {
            return;
        }
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone());
        let cache = std::env::temp_dir().join(format!("botport-api-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&cache);
        let mut cfg = ApiConfig::new("root", echo_token());
        cfg.base = base.clone();
        cfg.cache_dir = Some(cache.clone());
        let mut s = ApiStore::new(cfg.clone());
        s.put("HEAD", b"gen=1").unwrap();
        s.put("from-grok/state-1.json", b"{\"n\": 1}").unwrap();
        s.put("digest/hermes-1.md", b"digest").unwrap();
        assert_eq!(
            s.get("from-grok/state-1.json").unwrap().unwrap(),
            b"{\"n\": 1}"
        );
        // a new run: everything it reads is unchanged
        fake.lock().unwrap().calls.clear();
        let mut s2 = ApiStore::new(cfg);
        assert_eq!(s2.get("HEAD").unwrap().unwrap(), b"gen=1");
        assert_eq!(s2.list("from-grok").unwrap().len(), 1);
        assert_eq!(
            s2.get("from-grok/state-1.json").unwrap().unwrap(),
            b"{\"n\": 1}"
        );
        assert_eq!(s2.get("digest/hermes-1.md").unwrap().unwrap(), b"digest");
        let calls = fake.lock().unwrap().calls.clone();
        assert_eq!(
            calls,
            vec!["GET /drive/v3/files".to_string(); 2],
            "one listing per folder level (the pair folder, then its folders), no downloads"
        );
        // a file that changed is downloaded again
        let id = fake
            .lock()
            .unwrap()
            .files
            .iter()
            .find(|(_, v)| v.0 == "HEAD")
            .map(|(k, _)| k.clone())
            .unwrap();
        fake.lock().unwrap().files.get_mut(&id).unwrap().3 = b"gen=2".to_vec();
        let mut cfg3 = ApiConfig::new("root", echo_token());
        cfg3.base = base;
        cfg3.cache_dir = Some(cache.clone());
        assert_eq!(ApiStore::new(cfg3).get("HEAD").unwrap().unwrap(), b"gen=2");
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_refresh_sees_another_writer_and_moved_folders_are_not_used() {
        if Command::new("curl").arg("--version").output().is_err() {
            return;
        }
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone());
        let cache = std::env::temp_dir().join(format!("botport-api-cache2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&cache);
        let mut cfg = ApiConfig::new("root", echo_token());
        cfg.base = base.clone();
        cfg.cache_dir = Some(cache.clone());
        let mut s = ApiStore::new(cfg.clone());
        s.put("HEAD", b"gen=1").unwrap();
        s.put("to-grok/blobs/abc", b"x").unwrap();
        assert_eq!(s.get("HEAD").unwrap().unwrap(), b"gen=1");
        // another writer moves HEAD; this run still has the old listing, a refresh sees the change
        let id = fake
            .lock()
            .unwrap()
            .files
            .iter()
            .find(|(_, v)| v.0 == "HEAD")
            .map(|(k, _)| k.clone())
            .unwrap();
        fake.lock().unwrap().files.get_mut(&id).unwrap().3 = b"gen=2".to_vec();
        assert_eq!(s.get("HEAD").unwrap().unwrap(), b"gen=1");
        s.refresh();
        assert_eq!(s.get("HEAD").unwrap().unwrap(), b"gen=2");
        // the owner moves `to-grok` out of the pair folder: a new run never writes into it through a remembered id
        let tg = fake
            .lock()
            .unwrap()
            .files
            .iter()
            .find(|(_, v)| v.0 == "to-grok")
            .map(|(k, _)| k.clone())
            .unwrap();
        fake.lock().unwrap().files.get_mut(&tg).unwrap().1 = "elsewhere".into();
        let mut s2 = ApiStore::new(cfg);
        assert_eq!(s2.list("to-grok/blobs").unwrap(), vec![]);
        s2.put("to-grok/blobs/def", b"y").unwrap();
        let f = fake.lock().unwrap();
        let def = f.files.values().find(|v| v.0 == "def").unwrap();
        let moved_blobs = f
            .files
            .iter()
            .find(|(_, v)| v.0 == "blobs" && v.1 == tg)
            .map(|(k, _)| k.clone())
            .unwrap();
        assert_ne!(
            def.1, moved_blobs,
            "a new folder is made inside the pair folder instead"
        );
        drop(f);
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn files_someone_else_modified_are_ignored() {
        if Command::new("curl").arg("--version").output().is_err() {
            return;
        }
        let fake = Arc::new(Mutex::new(Fake::default()));
        fake.lock().unwrap().files.insert(
            "x1".into(),
            (
                "HEAD".into(),
                "root".into(),
                false,
                b"gen=9".to_vec(),
                false,
            ),
        );
        let base = serve(fake.clone());
        let mut s = store(&base, echo_token());
        assert_eq!(s.get("HEAD").unwrap(), None);
        assert!(s.list("").unwrap().is_empty());
    }

    #[test]
    fn bad_tokens_and_failures_never_show_the_token() {
        assert!(valid_token(TOKEN));
        for bad in ["", "a b", "a\"b", "a\nheader = x", "tok'en"] {
            assert!(!valid_token(bad), "{bad:?}");
        }
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone());
        // a token command that prints something else is refused before any call
        let mut s = store(
            &base,
            vec!["echo".into(), "not a token\nheader = \"X: y\"".into()],
        );
        assert!(s
            .get("HEAD")
            .unwrap_err()
            .contains("not a plain access token"));
        assert!(fake.lock().unwrap().calls.is_empty());
        let mut s = store(&base, vec!["/usr/bin/false".into()]);
        assert_eq!(s.get("HEAD").unwrap_err(), "token command failed");
        // a rejected token: the error names the status only
        let mut s = store(&base, vec!["echo".into(), "wrong-token".into()]);
        let e = s.list("").unwrap_err();
        assert_eq!(e, "Drive API list: HTTP 401");
        // the argv never holds the token
        let cfg = ApiConfig::new("root", echo_token());
        let a = curl_args(
            &cfg,
            "GET",
            "http://x/y",
            Some((Path::new("/tmp/b"), "text/plain")),
        );
        assert!(a.iter().all(|x| !x.contains(TOKEN)), "{a:?}");
        assert_eq!(a[0], "-q", "a user .curlrc is never read");
        assert!(
            a.windows(2).any(|w| w == ["-K", "-"]),
            "the config comes on stdin: {a:?}"
        );
        assert!(
            a.windows(2).any(|w| w == ["--proto", "=https"]),
            "the default base is https only"
        );
    }

    #[test]
    fn unowned_and_duplicate_files_are_not_trusted() {
        if Command::new("curl").arg("--version").output().is_err() {
            return;
        }
        let fake = Arc::new(Mutex::new(Fake::default()));
        fake.lock().unwrap().files.insert(
            "x2".into(),
            ("HEAD".into(), "root".into(), false, b"gen=1".to_vec(), true),
        );
        fake.lock().unwrap().files.insert(
            "x3".into(),
            ("HEAD".into(), "root".into(), false, b"gen=2".to_vec(), true),
        );
        let base = serve(fake.clone());
        let mut s = store(&base, echo_token());
        assert!(s.get("HEAD").unwrap_err().contains("same name"));
    }

    #[test]
    fn temp_dirs_remove_themselves() {
        let d = TmpDir::new().unwrap();
        let p = d.file("c", b"header = x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&d.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let dir = d.0.clone();
        drop(d);
        assert!(!dir.exists());
    }

    #[test]
    fn query_escaping() {
        assert_eq!(lit("a'b\\c"), "'a\\'b\\\\c'");
        assert_eq!(pct("a b/'"), "a%20b%2F%27");
    }
}

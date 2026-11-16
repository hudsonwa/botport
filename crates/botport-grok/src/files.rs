//! P3 files: list, download (never overwrites), upload, delete — engine commands and the helpers they share.
//!
//! - Download is the zero-touch twin route's data channel (a bot sends its export as a file), so it is built to be
//!   boring: the name the app gives is untrusted and is reduced to one safe path component; the file is created with
//!   `create_new` (an existing file is never replaced; a taken name gets ` (1)`, ` (2)`, ...); the bytes are written,
//!   synced, read back from disk and hashed. PASS needs the disk sha256 to equal the app's own sha256 of the bytes
//!   it handed over (when the app can compute one) and the size to equal the list's `sizeBytes` (second route:
//!   `files-list`). A file that fails the check is removed again (by its exact path) and the command exits 1.
//! - Large files: the debug-port transport moves bytes in base64 chunks ([`CHUNK`]), see `transport::cdp`.
use crate::engine::{halt, terr, Engine, ExitCode, Outcome, Proof, R};
use crate::model::*;
use crate::policy::{Action, BotsScope, Decision, Target};
use crate::transport::Cap;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bytes per chunk moved over the debug port (base64 makes each frame about 1.4 MB).
pub const CHUNK: usize = 1024 * 1024;
/// The app's upload limit (1.1.0: 25 MB per file).
pub const MAX_UPLOAD: u64 = 25 * 1024 * 1024;

pub fn sha256_hex(b: &[u8]) -> String {
    let d = Sha256::digest(b);
    d.iter().map(|x| format!("{x:02x}")).collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Strict base64 (standard alphabet, padding optional, whitespace ignored).
pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    let mut pad = 0;
    for ch in s.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return Err(format!("base64: bad byte {ch}")),
        };
        if pad > 0 {
            return Err("base64: data after padding".into());
        }
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Fetch `total` bytes in chunks of [`CHUNK`] through `fetch(offset, len) -> base64` and check each chunk's
/// length (a short or long chunk is an error naming the offset).
pub fn assemble(
    total: usize,
    mut fetch: impl FnMut(usize, usize) -> Result<String, String>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(total);
    let mut off = 0;
    while off < total {
        let n = CHUNK.min(total - off);
        let part = b64_decode(&fetch(off, n)?)?;
        if part.len() != n {
            return Err(format!(
                "chunk at byte {off}: got {} bytes, expected {n}",
                part.len()
            ));
        }
        out.extend_from_slice(&part);
        off += n;
    }
    Ok(out)
}

/// The app's file name reduced to one safe path component: no directories, no `..`, no leading dot, no control
/// characters or separators; empty becomes `file`. Never longer than 200 bytes (the extension is kept).
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, ':' | '/' | '\\' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() || s == ".." {
        s = "file".into();
    }
    if s.len() > 200 {
        let (stem, ext) = split_ext(&s);
        let ext = if ext.len() < 20 { ext } else { String::new() };
        let mut cut = 200 - ext.len();
        while !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        s = format!("{}{ext}", &stem[..cut]);
    }
    s
}

fn split_ext(s: &str) -> (String, String) {
    match s.rfind('.') {
        Some(i) if i > 0 => (s[..i].to_string(), s[i..].to_string()),
        _ => (s.to_string(), String::new()),
    }
}

/// Create a new file in `dir` for `name` (never an existing one): `name`, then `stem (1).ext`, ... Returns the
/// open file and its path. `create_new` makes the choice race-free.
pub fn create_unique(dir: &Path, name: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    let name = safe_name(name);
    let (stem, ext) = split_ext(&name);
    for i in 0..10_000 {
        let n = if i == 0 {
            name.clone()
        } else {
            format!("{stem} ({i}){ext}")
        };
        let p = dir.join(&n);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        // review L3: bot exports are private data, like every other botport file
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        match o.open(&p) {
            Ok(f) => return Ok((f, p)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free file name"))
}

/// Write `bytes` as a new file in `dir`, sync it, read it back and hash what is on disk.
pub fn save_new(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<(PathBuf, u64, String)> {
    save_new_checked(dir, name, bytes, |_, _| Ok(()))
}

/// The temporary name a download is written under before it is checked (review M5): a dot file, so
/// [`safe_name`] can never produce it as a final name.
pub fn partial_name(name: &str) -> String {
    format!(
        ".{}.botport-partial-{}",
        safe_name(name),
        std::process::id()
    )
}

/// [`save_new`], crash-safe (review M5): the bytes go to a new temporary file ([`partial_name`]), are synced, read
/// back and hashed; `check(size, sha256)` decides; only then is the file hard-linked under its final name (`link`
/// fails on an existing name, so nothing is ever overwritten and the ` (n)` choice stays race-free) and the
/// temporary name is removed. A crash leaves at most a `.…botport-partial-<pid>` file, never a truncated file
/// under the final name. A refusal by `check` comes back as an error of kind `InvalidData` carrying its reason.
pub fn save_new_checked(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    check: impl FnOnce(u64, &str) -> Result<(), String>,
) -> std::io::Result<(PathBuf, u64, String)> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(partial_name(name));
    // a leftover of an earlier crashed run of a process with this same pid (never a final name)
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp)?;
    let r = f.write_all(bytes).and_then(|_| f.sync_all());
    drop(f);
    let done = (|| {
        r?;
        let back = std::fs::read(&tmp)?;
        let (size, sha) = (back.len() as u64, sha256_hex(&back));
        check(size, &sha)
            .map_err(|why| std::io::Error::new(std::io::ErrorKind::InvalidData, why))?;
        let p = match link_unique(&tmp, dir, name) {
            Ok(p) => p,
            // a file system without hard links (exFAT, some network shares): the checked bytes are copied into
            // a new file (`create_new`, still never an existing one)
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                let (mut f, p) = create_unique(dir, name)?;
                let r = f.write_all(&back).and_then(|_| f.sync_all());
                if let Err(e) = r {
                    let _ = std::fs::remove_file(&p);
                    return Err(e);
                }
                p
            }
            Err(e) => return Err(e),
        };
        Ok((p, size, sha))
    })();
    let _ = std::fs::remove_file(&tmp);
    done
}

/// Hard-link `tmp` as a new file in `dir` for `name`: `name`, then `stem (1).ext`, ... (never an existing one).
fn link_unique(tmp: &Path, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let name = safe_name(name);
    let (stem, ext) = split_ext(&name);
    for i in 0..10_000 {
        let n = if i == 0 {
            name.clone()
        } else {
            format!("{stem} ({i}){ext}")
        };
        let p = dir.join(&n);
        match std::fs::hard_link(tmp, &p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free file name"))
}

/// Pick files by selector: exact name, exact path, or (non-destructive only) a unique case-insensitive name prefix.
pub fn pick<'a>(files: &'a [FileEntry], sel: &str, exact: bool) -> Result<&'a FileEntry, String> {
    if let Some(f) = files.iter().find(|f| f.name == sel || f.path == sel) {
        return Ok(f);
    }
    if exact {
        return Err(format!("no file named exactly {sel:?}"));
    }
    let l = sel.to_lowercase();
    let m: Vec<&FileEntry> = files
        .iter()
        .filter(|f| f.name.to_lowercase().starts_with(&l))
        .collect();
    match m.len() {
        1 => Ok(m[0]),
        0 => Err(format!("no file matches {sel:?}")),
        _ => Err(format!(
            "{sel:?} is ambiguous: {}",
            m.iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn content_type(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "log") => "text/plain",
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("jsonl") => "application/x-ndjson",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("db" | "sqlite") => "application/vnd.sqlite3",
        _ => "application/octet-stream",
    }
}

pub fn guess_content_type(p: &Path) -> String {
    content_type(p).to_string()
}

impl Engine {
    fn list_files(&mut self, what: &str, id: &str) -> R<Vec<FileEntry>> {
        let id = id.to_string();
        self.with_t(Cap::Files, |t| t.files(&id))
            .map_err(|e| terr(what, e))
    }

    /// `files <bot> list`.
    pub fn files(&mut self, sel: &str) -> Outcome<Vec<FileEntry>> {
        let w = self.read_warnings();
        let r = (|| {
            let what = "files";
            let bot = self.find(what, sel)?;
            self.decide(what, Action::Read, Some(&bot))?;
            let f = self.list_files(what, &bot.id)?;
            // second read of the list: the same names and sizes (the app answers from its file store)
            let again = self.list_files(what, &bot.id)?;
            let key = |v: &[FileEntry]| {
                let mut k: Vec<(String, u64)> =
                    v.iter().map(|f| (f.path.clone(), f.bytes)).collect();
                k.sort();
                k
            };
            if key(&f) != key(&again) {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    "the file list changed between two reads; run it again",
                    None,
                ));
            }
            let total: u64 = f.iter().map(|x| x.bytes).sum();
            let p = Proof::pass(
                format!("{} files, {total} bytes", f.len()),
                "self-check (two reads agree)",
                "self-check",
            );
            Ok((bot.id, Outcome::ok(f, p)))
        })();
        let (id, o) = match r {
            Ok((id, o)) => (Some(id), o),
            Err(h) => (None, Outcome::from_halt(h)),
        };
        let mut o = self.finish("files list", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    /// `files <bot> download [NAME...] [--out DIR]`: no names = every file. Never overwrites.
    pub fn file_download(
        &mut self,
        sel: &str,
        names: &[String],
        out: &Path,
    ) -> Outcome<Vec<SavedFile>> {
        let w = self.read_warnings();
        let mut saved: Vec<SavedFile> = vec![];
        let r = self.download_inner(sel, names, out, &mut saved);
        let o = match r {
            Ok((id, p)) => (Some(id), Outcome::ok(saved, p)),
            Err(h) => (None, {
                let mut o = Outcome::from_halt(h);
                if !saved.is_empty() {
                    o.value = Some(saved);
                }
                o
            }),
        };
        let mut o = self.finish("files download", o.0.as_deref(), o.1);
        o.warnings.extend(w);
        o
    }

    fn download_inner(
        &mut self,
        sel: &str,
        names: &[String],
        out: &Path,
        saved: &mut Vec<SavedFile>,
    ) -> R<(String, Proof)> {
        let what = "files download";
        let bot = self.find(what, sel)?;
        self.decide(what, Action::Read, Some(&bot))?;
        let list = self.list_files(what, &bot.id)?;
        let picked: Vec<FileEntry> = if names.is_empty() {
            list.clone()
        } else {
            names
                .iter()
                .map(|n| pick(&list, n, false).cloned())
                .collect::<Result<_, _>>()
                .map_err(|e| {
                    halt(
                        ExitCode::Other,
                        what,
                        e,
                        Some("botport grok files <bot> list"),
                    )
                })?
        };
        if picked.is_empty() {
            return Err(halt(
                ExitCode::Other,
                what,
                "the bot has no files",
                Some("botport grok files <bot> list"),
            ));
        }
        std::fs::create_dir_all(out)
            .map_err(|e| halt(ExitCode::Other, what, format!("output folder: {e}"), None))?;
        let id = bot.id.clone();
        // review L6: a file with no app sha256 and no listed size has nothing outside our own write to check
        let mut unverified: Vec<String> = vec![];
        for f in &picked {
            let (bytes, app_sha) = self
                .with_t(Cap::Files, |t| t.file_read_checked(&id, f))
                .map_err(|e| terr(&format!("download {}", f.name), e))?;
            let want = f.bytes;
            let app = app_sha.clone();
            let (path, size, sha) = save_new_checked(out, &f.name, &bytes, |size, sha| {
                if app.as_deref().is_some_and(|a| a != sha) {
                    Err("sha256 on disk differs from the app's sha256".to_string())
                } else if want > 0 && size != want {
                    Err(format!("{size} bytes on disk, the file list says {want}"))
                } else {
                    Ok(())
                }
            })
            .map_err(|e| {
                // a failed check leaves no file under the final name (only the temporary one, removed)
                let why = if e.kind() == std::io::ErrorKind::InvalidData {
                    e.to_string()
                } else {
                    format!("write: {e}")
                };
                halt(ExitCode::Other, &format!("download {}", f.name), why, None)
            })?;
            let shown = path.display().to_string();
            if app_sha.is_none() && f.bytes == 0 {
                unverified.push(f.name.clone());
            }
            saved.push(SavedFile {
                name: f.name.clone(),
                saved_to: shown,
                bytes: size,
                sha256: sha,
                app_sha256: app_sha,
            });
        }
        let n = saved.len();
        let total: u64 = saved.iter().map(|s| s.bytes).sum();
        let digest_checked = saved.iter().all(|s| s.app_sha256.is_some());
        let (what_s, check) = if n == 1 {
            (
                format!("{} saved, {} bytes", saved[0].name, saved[0].bytes),
                format!(
                    "sha256 {} on disk{}; size matches the file list",
                    &saved[0].sha256[..12],
                    if digest_checked { " = app's" } else { "" }
                ),
            )
        } else {
            (
                format!("{n} files saved, {total} bytes"),
                format!(
                    "sha256 on disk{} for each; sizes match the file list",
                    if digest_checked { " = app's" } else { "" }
                ),
            )
        };
        if !unverified.is_empty() {
            let check = format!(
                "self-check only: the app gave no sha256 and the file list no size for {}",
                unverified.join(", ")
            );
            return Ok((bot.id, Proof::pass(what_s, check, "self-check")));
        }
        Ok((bot.id, Proof::pass(what_s, check, "files-list")))
    }

    /// `files <bot> fetch [NAME|KEY] [--out DIR]`: a file the bot sent in chat (an attachment message), the newest
    /// one when no name is given. Never overwrites. PASS: the disk sha256 equals the app's sha256 of the bytes its
    /// preview loaded (`app-preview`).
    pub fn file_fetch(&mut self, sel: &str, which: Option<&str>, out: &Path) -> Outcome<SavedFile> {
        let w = self.read_warnings();
        let r = (|| {
            let what = "files fetch";
            let bot = self.find(what, sel)?;
            self.decide(what, Action::Read, Some(&bot))?;
            let msgs = self.read_raw(what, &bot.id, None, 0)?;
            let m = msgs
                .iter()
                .rev()
                .filter(|m| m.role == Role::Bot && m.file.is_some())
                .find(|m| {
                    which.is_none_or(|x| m.key == x || m.file.as_ref().is_some_and(|f| f.name == x))
                })
                .cloned()
                .ok_or_else(|| {
                    halt(
                        ExitCode::Other,
                        what,
                        match which {
                            Some(x) => format!("no file {x:?} sent by the bot in this chat"),
                            None => "the bot has not sent a file in this chat".into(),
                        },
                        Some("botport grok read <bot>"),
                    )
                })?;
            let f = m.file.clone().expect("filtered");
            let id = bot.id.clone();
            let key = m.key.clone();
            let (bytes, app_sha) = self
                .with_t(Cap::Files, |t| t.attachment_read(&id, &key, &f))
                .map_err(|e| terr(&format!("fetch {}", f.name), e))?;
            std::fs::create_dir_all(out)
                .map_err(|e| halt(ExitCode::Other, what, format!("output folder: {e}"), None))?;
            let Some(app) = app_sha else {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    "the app gave no sha256 to check against",
                    None,
                ));
            };
            let want = bytes.len() as u64;
            let (path, size, sha) = save_new_checked(out, &f.name, &bytes, |size, sha| {
                if app != sha || size != want {
                    Err("sha256 on disk differs from the app's".to_string())
                } else {
                    Ok(())
                }
            })
            .map_err(|e| {
                let why = if e.kind() == std::io::ErrorKind::InvalidData {
                    e.to_string()
                } else {
                    format!("write: {e}")
                };
                halt(ExitCode::Other, what, why, None)
            })?;
            let p = Proof::pass(
                format!("{} saved, {size} bytes", f.name),
                format!("sha256 {} on disk = app's", &sha[..12]),
                "app-preview",
            );
            Ok((
                id,
                Outcome::ok(
                    SavedFile {
                        name: f.name,
                        saved_to: path.display().to_string(),
                        bytes: size,
                        sha256: sha,
                        app_sha256: Some(app),
                    },
                    p,
                ),
            ))
        })();
        let (id, o) = match r {
            Ok((id, o)) => (Some(id), o),
            Err(h) => (None, Outcome::from_halt(h)),
        };
        let mut o = self.finish("files fetch", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    /// `files <bot> upload PATH...` (25 MB each). PASS: the list shows each with the same size.
    pub fn file_upload(&mut self, sel: &str, paths: &[PathBuf]) -> Outcome<Vec<FileEntry>> {
        let mut w = vec![];
        let r = (|| {
            let what = "files upload";
            if paths.is_empty() {
                return Err(halt(ExitCode::Other, what, "no file given", None));
            }
            for p in paths {
                // review M4: a symlink is not followed (it could point at any local file)
                let m = std::fs::symlink_metadata(p).map_err(|e| {
                    halt(ExitCode::Other, what, format!("{}: {e}", p.display()), None)
                })?;
                if m.file_type().is_symlink() {
                    return Err(halt(
                        ExitCode::Refused,
                        what,
                        format!("{} is a symlink; name the file itself", p.display()),
                        None,
                    ));
                }
                if !m.is_file() {
                    return Err(halt(
                        ExitCode::Other,
                        what,
                        format!("{} is not a file", p.display()),
                        None,
                    ));
                }
                if m.len() > MAX_UPLOAD {
                    return Err(halt(
                        ExitCode::Other,
                        what,
                        format!(
                            "{} is {} bytes; the app takes 25 MB per file",
                            p.display(),
                            m.len()
                        ),
                        None,
                    ));
                }
            }
            let bot = self.find(what, sel)?;
            self.decide(what, Action::Send, Some(&bot))?;
            self.version_gate(what, &mut w)?;
            let id = bot.id.clone();
            let mut up = vec![];
            for p in paths {
                let f = self
                    .with_t(Cap::Files, |t| t.file_attach(&id, p))
                    .map_err(|e| terr(what, e))?;
                up.push((f, std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
            }
            let list = self.list_files(what, &id)?;
            for (f, len) in &up {
                match list.iter().find(|x| x.path == f.path || x.name == f.name) {
                    Some(x) if x.bytes == *len => {}
                    Some(x) => {
                        return Err(halt(
                            ExitCode::Other,
                            what,
                            format!("{} is listed with {} bytes, sent {len}", f.name, x.bytes),
                            None,
                        ))
                    }
                    None => {
                        return Err(halt(
                            ExitCode::Other,
                            what,
                            format!("{} is not in the file list", f.name),
                            None,
                        ))
                    }
                }
            }
            let names: Vec<String> = up.iter().map(|(f, _)| f.name.clone()).collect();
            let p = Proof::pass(
                format!("uploaded {}", names.join(", ")),
                "file list shows each with the same size",
                "files-list",
            );
            Ok((id, Outcome::ok(up.into_iter().map(|x| x.0).collect(), p)))
        })();
        let (id, o) = match r {
            Ok((id, o)) => (Some(id), o),
            Err(h) => (None, Outcome::from_halt(h)),
        };
        let mut o = self.finish("files upload", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    /// `files <bot> delete NAME`: the exact name (or path) only (floor 4); over MCP `confirm` repeats it.
    pub fn file_delete(
        &mut self,
        sel: &str,
        exact: &str,
        confirm: Option<&str>,
        yes: bool,
    ) -> Outcome<FileEntry> {
        let mut w = vec![];
        let r = (|| {
            let what = "files delete";
            let bot = self.find(what, sel)?;
            self.decide(what, Action::Send, Some(&bot))?;
            let list = self.list_files(what, &bot.id)?;
            let f = pick(&list, exact, true)
                .map_err(|e| {
                    halt(
                        ExitCode::Refused,
                        what,
                        format!("refused by floor 4: {e}"),
                        Some("name the file exactly (its name or path)"),
                    )
                })?
                .clone();
            // review M6: the app deletes by name; with two files of that name it could remove the other one
            let same = list.iter().filter(|x| x.name == f.name).count();
            if same > 1 {
                return Err(halt(
                    ExitCode::Refused,
                    what,
                    format!(
                        "refused by floor 4: {same} files are named {:?}; the app deletes by name, so it cannot be exact",
                        f.name
                    ),
                    Some("rename or remove the other copies in Grok Bot first"),
                ));
            }
            self.decide_destructive(what, exact, confirm, yes, &f.path, &f.name)?;
            self.version_gate(what, &mut w)?;
            let id = bot.id.clone();
            let name = f.name.clone();
            self.with_t(Cap::Files, |t| t.file_remove(&id, &name))
                .map_err(|e| terr(what, e))?;
            let after = self.list_files(what, &id)?;
            if after.iter().any(|x| x.path == f.path) {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    format!("{} is still in the file list", f.name),
                    None,
                ));
            }
            // and nothing else went with it
            let gone: Vec<&str> = list
                .iter()
                .filter(|x| x.path != f.path && !after.iter().any(|y| y.path == x.path))
                .map(|x| x.name.as_str())
                .collect();
            if !gone.is_empty() {
                return Err(halt(
                    ExitCode::Other,
                    what,
                    format!("other files also left the list: {}", gone.join(", ")),
                    None,
                ));
            }
            let p = Proof::pass(
                format!("{} deleted", f.name),
                "absent from the file list",
                "files-list",
            );
            Ok((id, Outcome::ok(f, p)))
        })();
        let (id, o) = match r {
            Ok((id, o)) => (Some(id), o),
            Err(h) => (None, Outcome::from_halt(h)),
        };
        let mut o = self.finish("files delete", id.as_deref(), o);
        o.warnings.extend(w);
        o
    }

    /// The destructive check for a routine or file: floor 4 (exact id or name; over MCP `confirm` repeats it),
    /// `confirm_destructive` and the bot-callable refusal, decided on the item itself. The bot's own scope was
    /// decided before (so the allowlist is not applied to the item's name).
    pub(crate) fn decide_destructive(
        &mut self,
        what: &str,
        exact: &str,
        confirm: Option<&str>,
        yes: bool,
        item_id: &str,
        item_name: &str,
    ) -> R<()> {
        let mut p = self.policy.clone();
        p.bots_scope = BotsScope::All;
        let t = Target {
            id: item_id,
            name: item_name,
            main: false,
        };
        match p.decide(
            &self.caller,
            &Action::Delete {
                exact,
                confirm,
                yes,
            },
            Some(&t),
        ) {
            Decision::Allow => Ok(()),
            Decision::Ask(q) => {
                if (self.asker)(&q) {
                    Ok(())
                } else {
                    Err(halt(
                        ExitCode::Refused,
                        what,
                        format!("not confirmed: {q}"),
                        None,
                    ))
                }
            }
            Decision::Refuse { setting, hint } => Err(halt(
                ExitCode::Refused,
                what,
                format!("refused by {setting}"),
                Some(&hint),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip_all_lengths() {
        for n in 0..70 {
            let d: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&d)).unwrap(), d, "n={n}");
        }
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_decode("Zm9vYg==").unwrap(), b"foob");
        assert!(b64_decode("Zm9v!").is_err());
    }

    #[test]
    fn names_are_one_safe_component() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("..\\..\\x.txt"), "x.txt");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("a\u{0}b\nc.json"), "a_b_c.json");
        assert_eq!(safe_name("export.json"), "export.json");
        let long = format!("{}.json", "x".repeat(400));
        let s = safe_name(&long);
        assert!(s.len() <= 200 && s.ends_with(".json"));
    }

    #[test]
    fn chunks_assemble_and_a_short_chunk_fails() {
        let data: Vec<u8> = (0..(CHUNK * 2 + 123)).map(|i| (i % 251) as u8).collect();
        let got = assemble(data.len(), |o, n| Ok(b64_encode(&data[o..o + n]))).unwrap();
        assert_eq!(got, data);
        let e = assemble(data.len(), |o, n| {
            Ok(b64_encode(&data[o..o + n - usize::from(o > 0)]))
        })
        .unwrap_err();
        assert!(e.contains("chunk at byte"), "{e}");
    }
}

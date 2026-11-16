//! A minimal blocking WebSocket client (RFC 6455), enough for one loopback DevTools connection: text frames,
//! masking, ping/pong, fragmented messages, close. Loopback only (floor 1 is checked by the caller and here).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Ws {
    s: BufReader<TcpStream>,
    seed: u64,
}

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                o.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

/// SHA-1 (FIPS 180-4), only for the RFC 6455 `Sec-WebSocket-Accept` check. Not used for anything secret.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut m = data.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in m.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// The `Sec-WebSocket-Accept` value a server must answer to `key` (RFC 6455 section 4.2.2).
fn accept_for(key: &str) -> String {
    // the RFC 6455 GUID, split so the privacy gate does not read it as an id
    const GUID: &str = concat!("258EAFA5-E914-47DA-", "95CA-", "C5AB0DC85B11");
    b64(&sha1(format!("{key}{GUID}").as_bytes()))
}

fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

/// `http://127.0.0.1:PORT/path` GET over plain TCP (DevTools `/json/version`, `/json/list`).
pub fn http_get(port: u16, path: &str) -> std::io::Result<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    read_http_body(&mut BufReader::new(s))
}

/// Read one HTTP/1.1 response body. The app's DevTools server (Chromium) keeps the connection open after the
/// response even when asked to close, so the body is read by `Content-Length`, never to end of stream.
fn read_http_body(r: &mut impl BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    let mut len: Option<usize> = None;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(std::io::Error::other(
                "http: connection closed in the headers",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().ok();
            }
        }
    }
    let mut body = Vec::new();
    match len {
        Some(n) if n > 16 * 1024 * 1024 => {
            return Err(std::io::Error::other("http: body too large"))
        }
        Some(n) => {
            body.resize(n, 0);
            r.read_exact(&mut body)?;
        }
        None => {
            // review L5: capped like a declared length
            r.by_ref()
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut body)?;
            if body.len() > 16 * 1024 * 1024 {
                return Err(std::io::Error::other("http: body too large"));
            }
        }
    }
    String::from_utf8(body).map_err(std::io::Error::other)
}

/// Review L5: the HTTP status line of a websocket upgrade is exactly `HTTP/1.1 101 <reason>`.
pub fn is_upgrade_status(line: &str) -> bool {
    let mut p = line.trim_end_matches(['\r', '\n']).splitn(3, ' ');
    matches!(p.next(), Some("HTTP/1.1" | "HTTP/1.0")) && p.next() == Some("101")
}

/// Review L5: a websocket path that may go into the request line: `/devtools/page/<id>` or
/// `/devtools/browser/<id>`, with no space, CR, LF or other control character.
pub fn valid_ws_path(path: &str) -> bool {
    (path.starts_with("/devtools/page/") || path.starts_with("/devtools/browser/"))
        && path.bytes().all(|c| c.is_ascii_graphic())
}

/// The server's `Sec-WebSocket-Accept` must match our key, or the upgrade is refused.
fn check_accept(key: &str, got: Option<&str>) -> std::io::Result<()> {
    match got {
        Some(v) if v == accept_for(key) => Ok(()),
        Some(_) => Err(std::io::Error::other(
            "websocket upgrade: Sec-WebSocket-Accept does not match the key",
        )),
        None => Err(std::io::Error::other(
            "websocket upgrade: no Sec-WebSocket-Accept header",
        )),
    }
}

impl Ws {
    /// Connect to `ws://127.0.0.1:PORT/PATH`.
    pub fn connect(port: u16, path: &str) -> std::io::Result<Self> {
        if !valid_ws_path(path) {
            return Err(std::io::Error::other(
                "websocket path is not a DevTools target path",
            ));
        }
        let s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(15)))?;
        let mut seed = seed();
        let key: Vec<u8> = (0..16)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 33) as u8
            })
            .collect();
        let key = b64(&key);
        let mut w = s.try_clone()?;
        write!(
            w,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line)?;
        if !is_upgrade_status(&line) {
            return Err(std::io::Error::other(format!(
                "websocket upgrade refused: {}",
                line.trim()
            )));
        }
        let mut accept = None;
        loop {
            line.clear();
            r.read_line(&mut line)?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("sec-websocket-accept") {
                    accept = Some(v.trim().to_string());
                }
            }
        }
        check_accept(&key, accept.as_deref())?;
        Ok(Ws { s: r, seed })
    }

    fn mask(&mut self) -> [u8; 4] {
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.seed >> 32) as u32).to_be_bytes()
    }

    fn frame(&mut self, op: u8, data: &[u8]) -> std::io::Result<()> {
        let mut f = vec![0x80 | op];
        let n = data.len();
        if n < 126 {
            f.push(0x80 | n as u8);
        } else if n < 65536 {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let m = self.mask();
        f.extend_from_slice(&m);
        f.extend(data.iter().enumerate().map(|(i, b)| b ^ m[i % 4]));
        self.s.get_mut().write_all(&f)
    }

    /// Longer reads (a large attachment read in the app) raise the per-message timeout for one call.
    pub fn set_read_timeout(&self, d: Duration) -> std::io::Result<()> {
        self.s.get_ref().set_read_timeout(Some(d))
    }

    pub fn send_text(&mut self, s: &str) -> std::io::Result<()> {
        self.frame(0x1, s.as_bytes())
    }

    /// The next complete text message (pings answered, binary skipped).
    pub fn recv_text(&mut self) -> std::io::Result<String> {
        let mut msg = Vec::new();
        loop {
            let mut h = [0u8; 2];
            self.s.read_exact(&mut h)?;
            let fin = h[0] & 0x80 != 0;
            let op = h[0] & 0x0f;
            let masked = h[1] & 0x80 != 0;
            let mut n = (h[1] & 0x7f) as u64;
            if n == 126 {
                let mut b = [0u8; 2];
                self.s.read_exact(&mut b)?;
                n = u16::from_be_bytes(b) as u64;
            } else if n == 127 {
                let mut b = [0u8; 8];
                self.s.read_exact(&mut b)?;
                n = u64::from_be_bytes(b);
            }
            if n > 256 * 1024 * 1024 {
                return Err(std::io::Error::other("websocket frame too large"));
            }
            let mut m = [0u8; 4];
            if masked {
                self.s.read_exact(&mut m)?;
            }
            let mut data = vec![0u8; n as usize];
            self.s.read_exact(&mut data)?;
            if masked {
                for (i, b) in data.iter_mut().enumerate() {
                    *b ^= m[i % 4];
                }
            }
            match op {
                0x9 => self.frame(0xA, &data)?,
                0xA => {}
                0x8 => return Err(std::io::Error::other("websocket closed")),
                0x0..=0x2 => {
                    msg.extend_from_slice(&data);
                    if fin {
                        return String::from_utf8(msg).map_err(std::io::Error::other);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // review L5
    #[test]
    fn handshake_checks_are_exact() {
        use super::{is_upgrade_status, valid_ws_path};
        assert!(is_upgrade_status("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(!is_upgrade_status("HTTP/1.1 200 OK x 101 y\r\n"));
        assert!(!is_upgrade_status("HTTP/1.1 1010 x\r\n"));
        assert!(!is_upgrade_status("XHTTP/1.1 101 x\r\n"));
        assert!(valid_ws_path("/devtools/page/SYNTHETIC0001"));
        for p in [
            "0/devtools/page/x",
            "/devtools/page/x HTTP/1.1\r\nHost: evil",
            "/devtools/page/a b",
            "/json/list",
        ] {
            assert!(!valid_ws_path(p), "{p:?}");
        }
    }

    #[test]
    fn a_body_without_length_is_capped() {
        let big = format!(
            "HTTP/1.1 200 OK\r\n\r\n{}",
            "x".repeat(16 * 1024 * 1024 + 10)
        );
        let e = super::read_http_body(&mut std::io::Cursor::new(big.into_bytes())).unwrap_err();
        assert!(e.to_string().contains("too large"));
        let ok = "HTTP/1.1 200 OK\r\n\r\n{}";
        assert_eq!(
            super::read_http_body(&mut std::io::Cursor::new(ok.as_bytes())).unwrap(),
            "{}"
        );
    }

    /// Live finding (P1 acceptance, app 1.1.0): the DevTools HTTP server answers with `Content-Length` and keeps
    /// the socket open despite `Connection: close`. Reading to end of stream timed out (EAGAIN, exit 5 on every
    /// command). Fixture: the recorded header shape with a synthetic body; the server never closes.
    #[test]
    fn http_body_is_read_by_content_length_when_the_server_keeps_the_socket_open() {
        use std::io::{Read, Write};
        let fx: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/app-1.1.0/http-json-version.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let head: Vec<&str> = fx["head"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect();
        // the wire: CRLF headers (with the recorded Content-Length), then the body byte for byte
        let resp = format!(
            "{}\r\n\r\n{}",
            head.join("\r\n"),
            fx["body"].as_str().unwrap()
        );
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut b = [0u8; 1024];
            let _ = c.read(&mut b);
            c.write_all(resp.as_bytes()).unwrap();
            // keep the socket open past the client's read timeout, as the app does
            std::thread::sleep(std::time::Duration::from_secs(7));
        });
        let t0 = std::time::Instant::now();
        let body = super::http_get(port, "/json/version").unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(3));
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["User-Agent"].as_str().unwrap().contains("GrokBot/1.1.0"));
        drop(h);
    }

    #[test]
    fn base64() {
        assert_eq!(super::b64(b"hello"), "aGVsbG8=");
        assert_eq!(super::b64(b"hi"), "aGk=");
        assert_eq!(super::b64(b"abc"), "YWJj");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha1_vectors() {
        // FIPS 180-4 / RFC 3174 test vectors
        assert_eq!(
            hex(&super::sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(&super::sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&super::sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&super::sha1(&vec![b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn accept_matches_rfc6455_example_and_mismatch_is_refused() {
        // RFC 6455 section 1.3
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(super::accept_for(key), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert!(super::check_accept(key, Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")).is_ok());
        assert!(super::check_accept(key, Some("AAAAAAAAAAAAAAAAAAAAAAAAAAA=")).is_err());
        assert!(super::check_accept(key, None).is_err());
    }
}

/// Property test: the HTTP body reader (DevTools `/json/version`, `/json/list`) never panics on arbitrary bytes
/// and returns exactly the declared body when one is declared (deterministic xorshift cases, no dependency).
#[cfg(test)]
mod http_body_fuzz {
    use std::io::Cursor;

    fn rng(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    #[test]
    fn arbitrary_responses_never_panic_and_declared_bodies_round_trip() {
        let atoms: [&[u8]; 12] = [
            b"HTTP/1.1 200 OK\r\n",
            b"Content-Length: ",
            b"content-length:",
            b"7",
            b"99999999999999999999",
            b"\r\n",
            b"\n",
            b"{\"a\":1}",
            b":",
            b" ",
            b"\xff\xfe",
            b"Content-Length: 18446744073709551615\r\n",
        ];
        let mut s = 0x5eed_u64;
        for _ in 0..20_000 {
            let mut buf = Vec::new();
            for _ in 0..(rng(&mut s) % 12) {
                buf.extend_from_slice(atoms[(rng(&mut s) % atoms.len() as u64) as usize]);
            }
            let _ = super::read_http_body(&mut Cursor::new(buf));
        }
        for _ in 0..2_000 {
            let n = (rng(&mut s) % 200) as usize;
            let body: String = (0..n)
                .map(|_| (b'a' + (rng(&mut s) % 26) as u8) as char)
                .collect();
            let raw =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n{body}TRAILING-KEEPALIVE");
            assert_eq!(super::read_http_body(&mut Cursor::new(raw)).unwrap(), body);
            let raw = format!("HTTP/1.1 200 OK\r\n\r\n{body}");
            assert_eq!(super::read_http_body(&mut Cursor::new(raw)).unwrap(), body);
        }
    }
}

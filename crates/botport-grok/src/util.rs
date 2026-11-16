//! Small helpers: times, the botport home directory, private (0600) files.
use std::io::Write;
use std::path::{Path, PathBuf};

/// RFC 3339 UTC for a Unix time in milliseconds, e.g. `2026-11-16T03:52:10Z`.
pub fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        (s / 60) % 60,
        s % 60
    )
}

/// `HH:MMZ` (UTC) for a Unix time in milliseconds, for short PASS lines.
pub fn hhmm(ms: u64) -> String {
    let s = (ms / 1000) % 86_400;
    format!("{:02}:{:02}Z", s / 3600, (s / 60) % 60)
}

/// Human output: local wall time with its zone, e.g. `2026-11-16 11:59:35 AWST`. JSON keeps [`rfc3339`] (UTC).
pub fn local(ms: u64) -> String {
    let (off, zone) = local_offset(ms);
    fmt_local(ms, off, &zone)
}

/// Human output: `HH:MM` local with its zone, e.g. `12:59 AWST`, for short PASS lines.
pub fn hhmm_local(ms: u64) -> String {
    let (off, zone) = local_offset(ms);
    let s = (ms / 1000) as i64 + off;
    let d = s.rem_euclid(86_400);
    format!("{:02}:{:02} {zone}", d / 3600, (d / 60) % 60)
}

/// `ms` shifted by `offset_s` and labelled with `zone` (pure; [`local`] supplies the system zone).
pub fn fmt_local(ms: u64, offset_s: i64, zone: &str) -> String {
    let s = (ms / 1000) as i64 + offset_s;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} {zone}",
        t / 3600,
        (t / 60) % 60,
        t % 60
    )
}

/// An RFC 3339 time from our own JSON (`2026-11-16T03:59:35Z`, optional fraction or offset) as Unix ms.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = &s[19..];
    let mut frac_ms = 0i64;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
        frac_ms = format!("{:0<3}", &digits[..digits.len().min(3)])
            .parse()
            .ok()?;
        rest = &r[digits.len()..];
    }
    let off = match rest {
        "Z" | "z" => 0,
        // review L2: ASCII only, so the byte slices below cannot cut a character
        o if o.len() == 6 && o.is_ascii() && (o.starts_with('+') || o.starts_with('-')) => {
            let v = o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60;
            if o.starts_with('-') {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + se - off;
    u64::try_from(secs * 1000 + frac_ms).ok()
}

/// A JSON time string for human output: local with zone; passed through unchanged if it does not parse.
pub fn local_from_rfc3339(s: &str) -> String {
    parse_rfc3339(s).map(local).unwrap_or_else(|| s.to_string())
}

/// The system's UTC offset (seconds) and zone abbreviation at `ms` (`localtime_r`; honours `TZ`).
#[cfg(unix)]
fn local_offset(ms: u64) -> (i64, String) {
    // `struct tm` on macOS and glibc/musl: nine ints, then tm_gmtoff (long) and tm_zone (char*).
    #[repr(C)]
    struct Tm {
        ints: [i32; 9],
        gmtoff: std::ffi::c_long,
        zone: *const std::ffi::c_char,
    }
    extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let t = (ms / 1000) as i64;
    let mut tm = Tm {
        ints: [0; 9],
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: localtime_r writes only into `tm`; tm_zone points at static tz storage that we copy at once.
    unsafe {
        if localtime_r(&t, &mut tm).is_null() {
            return (0, "UTC".into());
        }
        let zone = if tm.zone.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(tm.zone)
                .to_string_lossy()
                .into_owned()
        };
        let off = tm.gmtoff as i64;
        let zone = if zone.is_empty() {
            fmt_offset(off)
        } else {
            zone
        };
        (off, zone)
    }
}

#[cfg(not(unix))]
fn local_offset(_ms: u64) -> (i64, String) {
    (0, "UTC".into())
}

#[cfg(any(unix, test))]
fn fmt_offset(off: i64) -> String {
    let a = off.abs();
    format!(
        "{}{:02}:{:02}",
        if off < 0 { '-' } else { '+' },
        a / 3600,
        (a / 60) % 60
    )
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `$BOTPORT_HOME`, else `~/.botport`. Holds the port file, the policy and the action log.
pub fn botport_home() -> PathBuf {
    if let Some(h) = std::env::var_os("BOTPORT_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".botport")
}

/// Create `dir` (0700 on unix) if missing. A folder that already exists is left as it is (review L4:
/// `BOTPORT_HOME=$HOME` must not lock down the home folder).
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `bytes` to `path` as a 0600 file (created 0600, never world-readable in between), atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        private_dir(d)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Append one line to a 0600 file.
pub fn append_private(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        private_dir(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    // review L4: a file that existed with wider permissions is tightened too
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    writeln!(f, "{line}")
}

/// Review L7: an exclusive advisory lock on `path` (created 0600), held until the returned file is dropped.
/// Blocks while another botport process holds it.
pub fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(d) = path.parent() {
        private_dir(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let f = o.open(path)?;
    f.lock()?;
    Ok(f)
}

/// Unix time now in milliseconds (wall clock; the engine uses its injected [`crate::clock::Clock`]).
pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse `90s`, `30m`, `2h` or a bare number of seconds.
pub fn parse_duration_s(s: &str) -> Option<u64> {
    let s = s.trim();
    let (n, mul) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    // review L1: no `inf`, `nan` or values that would saturate (more than ~31 years)
    n.parse::<f64>()
        .ok()
        .map(|v| v * mul as f64)
        .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 1e9)
        .map(|v| v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_794_801_575_010), "2026-11-16T03:59:35Z");
        assert_eq!(hhmm(1_794_801_575_010), "03:59Z");
        assert_eq!(parse_duration_s("30m"), Some(1800));
        assert_eq!(parse_duration_s("8"), Some(8));
        assert_eq!(parse_duration_s("x"), None);
    }

    // review L1, L2
    #[test]
    fn bad_durations_and_offsets_are_rejected_without_panics() {
        for s in ["1e30s", "infs", "inf", "nan", "NaNm", "-5", "1e10h"] {
            assert_eq!(parse_duration_s(s), None, "{s}");
        }
        assert_eq!(parse_duration_s("24h"), Some(86_400));
        for s in [
            "2026-11-16T03:59:35+1\u{e9}12",
            "2026-11-16T03:59:35\u{e9}0:00",
            "2026-11-16T03:59:35+08:\u{e9}",
        ] {
            assert_eq!(parse_rfc3339(s), None, "{s}");
        }
    }

    #[test]
    fn local_times_carry_their_zone_and_round_trip() {
        let t = 1_794_801_575_010;
        assert_eq!(fmt_local(t, 8 * 3600, "AWST"), "2026-11-16 11:59:35 AWST");
        assert_eq!(fmt_local(t, -6 * 3600, "CST"), "2026-11-15 21:59:35 CST");
        assert_eq!(fmt_local(t, 0, "UTC"), "2026-11-16 03:59:35 UTC");
        assert_eq!(
            parse_rfc3339("2026-11-16T03:59:35Z"),
            Some(1_794_801_575_000)
        );
        assert_eq!(parse_rfc3339("2026-11-16T03:59:35.010Z"), Some(t));
        assert_eq!(
            parse_rfc3339("2026-11-16T11:59:35+08:00"),
            Some(1_794_801_575_000)
        );
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("nope"), None);
        for ms in [0u64, 951_782_400_000, t] {
            assert_eq!(parse_rfc3339(&rfc3339(ms)), Some(ms / 1000 * 1000));
        }
        // the system zone: a non-empty label, and the same instant as the UTC form
        let l = local(t);
        let zone = l.rsplit(' ').next().unwrap();
        assert!(!zone.is_empty(), "{l}");
        assert!(hhmm_local(t).ends_with(zone));
        assert_eq!(local_from_rfc3339("garbage"), "garbage");
        assert_eq!(fmt_offset(-(5 * 3600 + 1800)), "-05:30");
    }
}

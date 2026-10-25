//! Time zones for the `Intl` polyfill: the name of the local IANA zone and
//! UTC offsets of named zones, read from the system tz database (TZif files
//! in `/usr/share/zoneinfo` and friends; RFC 8536). Systems without one
//! (Windows, slim containers) and zones it lacks use the copy embedded by
//! `jiff-tzdb`.
//!
//! Scripts can only learn offsets and abbreviations: zone names are
//! validated, lookups stay inside the zoneinfo directories, and only files
//! starting with the TZif magic are parsed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const ZONEINFO_DIRS: &[&str] =
    &["/usr/share/zoneinfo", "/var/db/timezone/zoneinfo", "/usr/lib/zoneinfo", "/usr/share/lib/zoneinfo"];
const MAX_TZIF_BYTES: u64 = 256 * 1024;
const MAX_CACHED_ZONES: usize = 256;

#[derive(Clone, Debug)]
struct LocalType {
    utoff: i32,
    isdst: bool,
    abbr: String,
}

/// A parsed TZif file.
#[derive(Debug)]
pub(crate) struct Zone {
    /// Canonical spelling of the name (the file's path in the database).
    pub name: String,
    transitions: Vec<i64>,
    type_idx: Vec<u8>,
    types: Vec<LocalType>,
    footer: Option<PosixTz>,
}

/// A POSIX TZ rule such as `EST5EDT,M3.2.0,M11.1.0` (offsets in seconds
/// east of UTC).
#[derive(Clone, Debug)]
struct PosixTz {
    std_abbr: String,
    std_off: i32,
    dst: Option<(String, i32, Rule, Rule)>,
}

#[derive(Clone, Copy, Debug)]
enum Day {
    /// `Jn`: day 1..=365, February 29 never counted.
    Julian1(u16),
    /// `n`: day 0..=365, counting February 29.
    Julian0(u16),
    /// `Mm.w.d`: weekday `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    Month(u8, u8, u8),
}

#[derive(Clone, Copy, Debug)]
struct Rule {
    day: Day,
    /// Local time of day of the transition, in seconds (may exceed a day).
    time: i32,
}

/// The local zone: `TZ` when it names a zone, otherwise the system setting.
pub(crate) fn local_name() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':');
        if let Some(z) = zone(tz) {
            return Some(z.name.clone());
        }
    }
    iana_time_zone::get_timezone().ok().filter(|n| !n.is_empty())
}

/// Local time of a named zone at an instant.
#[derive(Debug)]
pub(crate) struct ZoneTime {
    /// Seconds east of UTC.
    pub offset: i32,
    pub abbr: String,
    pub is_dst: bool,
    /// Canonical spelling of the zone name.
    pub name: String,
}

/// The local time type of `name` at `unix_secs`, or `None` for unknown zones.
pub(crate) fn offset_at(name: &str, unix_secs: i64) -> Option<ZoneTime> {
    let z = zone(name)?;
    let t = z.local_type(unix_secs)?;
    Some(ZoneTime { offset: t.utoff, abbr: t.abbr, is_dst: t.isdst, name: z.name.clone() })
}

fn cache() -> &'static Mutex<HashMap<String, Option<Arc<Zone>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<Zone>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn zone(name: &str) -> Option<Arc<Zone>> {
    if !valid_name(name) {
        return None;
    }
    let key = name.to_ascii_lowercase();
    if let Some(hit) = cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }
    let loaded = load(name).map(Arc::new);
    if let Ok(mut c) = cache().lock() {
        if c.len() >= MAX_CACHED_ZONES {
            c.clear();
        }
        c.insert(key, loaded.clone());
    }
    loaded
}

/// `Area/Location` style names only: no empty or dot segments, no
/// database metadata files.
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let first = name.split('/').next().unwrap_or_default().to_ascii_lowercase();
    if matches!(first.as_str(), "posix" | "right" | "posixrules" | "localtime" | "etc/localtime") {
        return false;
    }
    name.split('/').all(|seg| {
        !seg.is_empty()
            && !seg.starts_with('.')
            && !seg.starts_with('-')
            && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
    })
}

/// Finds the file for `name` (matching case-insensitively, as `Intl` does)
/// and returns its canonical spelling.
fn find(name: &str) -> Option<(PathBuf, String)> {
    for dir in ZONEINFO_DIRS {
        let mut path = PathBuf::from(dir);
        let mut canonical = Vec::new();
        let mut ok = true;
        for seg in name.split('/') {
            let found = std::fs::read_dir(&path).ok().and_then(|entries| {
                let mut ci = None;
                for e in entries.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n == seg {
                        return Some(n);
                    }
                    if ci.is_none() && n.eq_ignore_ascii_case(seg) {
                        ci = Some(n);
                    }
                }
                ci
            });
            match found {
                Some(n) => {
                    path.push(&n);
                    canonical.push(n);
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && path.is_file() {
            return Some((path, canonical.join("/")));
        }
    }
    None
}

fn load(name: &str) -> Option<Zone> {
    let from_system = || {
        let (path, canonical) = find(name)?;
        let meta = std::fs::metadata(&path).ok()?;
        if meta.len() > MAX_TZIF_BYTES {
            return None;
        }
        let data = std::fs::read(&path).ok()?;
        parse_tzif(&data, canonical)
    };
    from_system().or_else(|| load_embedded(name))
}

/// `name` from the embedded database (looked up ignoring case).
fn load_embedded(name: &str) -> Option<Zone> {
    let (canonical, data) = jiff_tzdb::get(name)?;
    parse_tzif(data, canonical.to_string())
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn header(r: &mut Reader) -> Option<Header> {
    if r.take(4)? != b"TZif" {
        return None;
    }
    let version = r.take(1)?[0];
    r.take(15)?;
    let mut n = [0usize; 6];
    for v in &mut n {
        *v = r.u32()? as usize;
    }
    Some(Header { version, isutcnt: n[0], isstdcnt: n[1], leapcnt: n[2], timecnt: n[3], typecnt: n[4], charcnt: n[5] })
}

fn parse_tzif(data: &[u8], name: String) -> Option<Zone> {
    let mut r = Reader { data, pos: 0 };
    let mut h = header(&mut r)?;
    let mut time_size = 4;
    if h.version >= b'2' {
        // Skip the 32-bit block; the 64-bit one follows with its own header.
        let skip = h.timecnt * 5 + h.typecnt * 6 + h.charcnt + h.leapcnt * 8 + h.isstdcnt + h.isutcnt;
        r.take(skip)?;
        h = header(&mut r)?;
        time_size = 8;
    }
    if h.typecnt == 0 || h.typecnt > 256 {
        return None;
    }
    let mut transitions = Vec::with_capacity(h.timecnt);
    for _ in 0..h.timecnt {
        let b = r.take(time_size)?;
        transitions.push(if time_size == 8 {
            i64::from_be_bytes(b.try_into().ok()?)
        } else {
            i64::from(i32::from_be_bytes(b.try_into().ok()?))
        });
    }
    let type_idx = r.take(h.timecnt)?.to_vec();
    let mut raw_types = Vec::with_capacity(h.typecnt);
    for _ in 0..h.typecnt {
        let b = r.take(6)?;
        raw_types.push((i32::from_be_bytes([b[0], b[1], b[2], b[3]]), b[4] != 0, b[5] as usize));
    }
    let chars = r.take(h.charcnt)?;
    let types = raw_types
        .into_iter()
        .map(|(utoff, isdst, idx)| {
            let tail = chars.get(idx..).unwrap_or_default();
            let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
            LocalType { utoff, isdst, abbr: String::from_utf8_lossy(&tail[..end]).into_owned() }
        })
        .collect::<Vec<_>>();
    if type_idx.iter().any(|&i| i as usize >= types.len()) {
        return None;
    }
    let mut footer = None;
    if h.version >= b'2' {
        r.take(h.leapcnt * (time_size + 4) + h.isstdcnt + h.isutcnt)?;
        let rest = &data[r.pos..];
        if rest.first() == Some(&b'\n')
            && let Some(end) = rest[1..].iter().position(|&c| c == b'\n')
        {
            footer = std::str::from_utf8(&rest[1..1 + end]).ok().and_then(parse_posix);
        }
    }
    Some(Zone { name, transitions, type_idx, types, footer })
}

impl Zone {
    fn local_type(&self, t: i64) -> Option<LocalType> {
        if self.transitions.is_empty() || t < self.transitions[0] {
            if self.transitions.is_empty()
                && let Some(f) = &self.footer
            {
                return Some(f.local_type(t));
            }
            // Before the first transition: the first standard-time type.
            return self.types.iter().find(|t| !t.isdst).or(self.types.first()).cloned();
        }
        let last = *self.transitions.last()?;
        if t >= last
            && let Some(f) = &self.footer
        {
            return Some(f.local_type(t));
        }
        let i = self.transitions.partition_point(|&x| x <= t) - 1;
        self.types.get(self.type_idx[i] as usize).cloned()
    }
}

/// Parses `std offset [dst [offset] [,start[/time],end[/time]]]`.
fn parse_posix(s: &str) -> Option<PosixTz> {
    let b = s.as_bytes();
    let mut i = 0;
    let std_abbr = posix_abbr(b, &mut i)?;
    let std_off = -posix_offset(b, &mut i)?;
    if i == b.len() {
        return Some(PosixTz { std_abbr, std_off, dst: None });
    }
    let dst_abbr = posix_abbr(b, &mut i)?;
    let dst_off = if i < b.len() && b[i] != b',' { -posix_offset(b, &mut i)? } else { std_off + 3600 };
    if i == b.len() {
        // No rule: the POSIX default (US rules) is rare enough to ignore.
        return Some(PosixTz { std_abbr, std_off, dst: None });
    }
    if b[i] != b',' {
        return None;
    }
    i += 1;
    let start = posix_rule(b, &mut i)?;
    if b.get(i) != Some(&b',') {
        return None;
    }
    i += 1;
    let end = posix_rule(b, &mut i)?;
    (i == b.len()).then_some(PosixTz { std_abbr, std_off, dst: Some((dst_abbr, dst_off, start, end)) })
}

fn posix_abbr(b: &[u8], i: &mut usize) -> Option<String> {
    let start = *i;
    if b.get(*i) == Some(&b'<') {
        let close = b[*i..].iter().position(|&c| c == b'>')? + *i;
        *i = close + 1;
        return Some(String::from_utf8_lossy(&b[start + 1..close]).into_owned());
    }
    while *i < b.len() && b[*i].is_ascii_alphabetic() {
        *i += 1;
    }
    (*i - start >= 3).then(|| String::from_utf8_lossy(&b[start..*i]).into_owned())
}

fn posix_number(b: &[u8], i: &mut usize) -> Option<i32> {
    let start = *i;
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
    }
    std::str::from_utf8(&b[start..*i]).ok()?.parse().ok()
}

/// `[+-]hh[:mm[:ss]]` in seconds (POSIX sign: positive is west of UTC).
fn posix_offset(b: &[u8], i: &mut usize) -> Option<i32> {
    let sign = match b.get(*i) {
        Some(b'-') => {
            *i += 1;
            -1
        }
        Some(b'+') => {
            *i += 1;
            1
        }
        _ => 1,
    };
    let mut secs = posix_number(b, i)? * 3600;
    for mul in [60, 1] {
        if b.get(*i) == Some(&b':') {
            *i += 1;
            secs += posix_number(b, i)? * mul;
        } else {
            break;
        }
    }
    Some(sign * secs)
}

fn posix_rule(b: &[u8], i: &mut usize) -> Option<Rule> {
    let day = match b.get(*i)? {
        b'J' => {
            *i += 1;
            Day::Julian1(posix_number(b, i)?.clamp(1, 365) as u16)
        }
        b'M' => {
            *i += 1;
            let m = posix_number(b, i)?;
            (b.get(*i) == Some(&b'.')).then_some(())?;
            *i += 1;
            let w = posix_number(b, i)?;
            (b.get(*i) == Some(&b'.')).then_some(())?;
            *i += 1;
            let d = posix_number(b, i)?;
            if !(1..=12).contains(&m) || !(1..=5).contains(&w) || !(0..=6).contains(&d) {
                return None;
            }
            Day::Month(m as u8, w as u8, d as u8)
        }
        _ => Day::Julian0(posix_number(b, i)?.clamp(0, 365) as u16),
    };
    let time = if b.get(*i) == Some(&b'/') {
        *i += 1;
        posix_offset(b, i)?
    } else {
        7200
    };
    Some(Rule { day, time })
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn year_of(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + i64::from(m <= 2)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl Rule {
    /// Seconds since the epoch of the transition in `year`, in local time.
    fn local_secs(&self, year: i64) -> i64 {
        let day = match self.day {
            Day::Julian1(n) => {
                let n = i64::from(n);
                days_from_civil(year, 1, 1) + n - 1 + i64::from(is_leap(year) && n >= 60)
            }
            Day::Julian0(n) => days_from_civil(year, 1, 1) + i64::from(n),
            Day::Month(m, w, d) => {
                let first = days_from_civil(year, u32::from(m), 1);
                let wd = (first + 4).rem_euclid(7);
                let mut day = 1 + (i64::from(d) - wd).rem_euclid(7) + (i64::from(w) - 1) * 7;
                let dim = i64::from(days_in_month(year, u32::from(m)));
                while day > dim {
                    day -= 7;
                }
                first + day - 1
            }
        };
        day * 86_400 + i64::from(self.time)
    }
}

impl PosixTz {
    fn local_type(&self, t: i64) -> LocalType {
        let std = LocalType { utoff: self.std_off, isdst: false, abbr: self.std_abbr.clone() };
        let Some((abbr, dst_off, start, end)) = &self.dst else { return std };
        let year = year_of((t + i64::from(self.std_off)).div_euclid(86_400));
        // Transitions happen at local wall time: the start in standard
        // time, the end in daylight time.
        let start_utc = start.local_secs(year) - i64::from(self.std_off);
        let end_utc = end.local_secs(year) - i64::from(*dst_off);
        let in_dst = if start_utc < end_utc { t >= start_utc && t < end_utc } else { !(t >= end_utc && t < start_utc) };
        if in_dst { LocalType { utoff: *dst_off, isdst: true, abbr: abbr.clone() } } else { std }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_rules() {
        let ny = parse_posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2026-03-08 06:59:59 UTC is still EST; 07:00 UTC (02:00 EST) is EDT.
        let start = days_from_civil(2026, 3, 8) * 86_400 + 7 * 3600;
        assert_eq!(ny.local_type(start - 1).abbr, "EST");
        assert_eq!(ny.local_type(start).abbr, "EDT");
        assert_eq!(ny.local_type(start).utoff, -4 * 3600);
        // Ends 2026-11-01 02:00 EDT = 06:00 UTC.
        let end = days_from_civil(2026, 11, 1) * 86_400 + 6 * 3600;
        assert_eq!(ny.local_type(end - 1).abbr, "EDT");
        assert_eq!(ny.local_type(end).abbr, "EST");

        // Southern hemisphere, quoted abbreviations and minutes.
        let syd = parse_posix("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(syd.local_type(days_from_civil(2026, 1, 15) * 86_400).utoff, 11 * 3600);
        assert_eq!(syd.local_type(days_from_civil(2026, 7, 15) * 86_400).utoff, 10 * 3600);
        let fixed = parse_posix("<+0530>-5:30").unwrap();
        assert_eq!(fixed.local_type(0).utoff, 5 * 3600 + 1800);
        assert_eq!(fixed.local_type(0).abbr, "+0530");
        assert!(parse_posix("E5").is_none());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for d in [-800_000, -1, 0, 59, 11_016, 20_000, 2_000_000] {
            let y = year_of(d);
            assert!(days_from_civil(y, 1, 1) <= d && d < days_from_civil(y + 1, 1, 1), "{d} -> {y}");
        }
    }

    #[test]
    fn names_are_validated() {
        assert!(valid_name("America/Argentina/Buenos_Aires"));
        assert!(valid_name("Etc/GMT+5"));
        for bad in ["", "../etc/passwd", "America/../../x", "/etc/passwd", ".hidden", "a//b", "posix/UTC", "a b"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn embedded_database() {
        let z = load_embedded("america/new_york").unwrap();
        assert_eq!(z.name, "America/New_York");
        let t = z.local_type(1_782_000_000).unwrap();
        assert_eq!((t.utoff, t.abbr.as_str(), t.isdst), (-4 * 3600, "EDT", true));
        let t = z.local_type(1_767_225_600).unwrap();
        assert_eq!((t.utoff, t.isdst), (-5 * 3600, false));
        // Far beyond the transitions: the POSIX footer rule.
        let t = z.local_type(4_102_444_800 + 180 * 86_400).unwrap();
        assert_eq!(t.utoff, -4 * 3600);
        assert_eq!(load_embedded("Asia/Kolkata").unwrap().local_type(0).unwrap().utoff, 5 * 3600 + 1800);
        assert!(load_embedded("Mars/Olympus").is_none());
    }

    #[test]
    fn system_database() {
        // Only where the system has a tz database.
        if find("UTC").is_none() {
            return;
        }
        let t = offset_at("america/new_york", 1_782_000_000).unwrap();
        assert_eq!(
            (t.offset, t.abbr.as_str(), t.name.as_str(), t.is_dst),
            (-4 * 3600, "EDT", "America/New_York", true)
        );
        let t = offset_at("America/New_York", 1_767_225_600).unwrap();
        assert_eq!((t.offset, t.is_dst), (-5 * 3600, false));
        assert_eq!(offset_at("Asia/Kolkata", 0).map(|o| o.offset), Some(5 * 3600 + 1800));
        assert!(offset_at("Mars/Olympus", 0).is_none());
        assert!(offset_at("../../etc/passwd", 0).is_none());
    }
}

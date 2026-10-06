//! iCalendar (RFC 5545) reading: just enough to answer "what is on, and when?" for a calendar feed.
//!
//! Supported: line unfolding and text escapes, `VEVENT` with UTC / floating / `TZID` / all-day times
//! (time zones come from the feed's own `VTIMEZONE` rules, evaluated here; floating times and zones
//! the feed does not define use the platform's local zone through [`LocalZone`]), `DURATION`,
//! `RRULE` (DAILY / WEEKLY / MONTHLY / YEARLY with INTERVAL, COUNT, UNTIL, BYDAY incl. ordinals,
//! BYMONTHDAY incl. negatives, BYMONTH, BYSETPOS, WKST), `RDATE`, `EXDATE`, `RECURRENCE-ID`
//! overrides and `STATUS:CANCELLED`. Not supported (the event then appears once, at its first
//! date): BYWEEKNO, BYYEARDAY and sub-daily frequencies.
//!
//! The input is *untrusted* (invitations from other people end up in a feed): nothing here panics on
//! malformed text, every loop is bounded, and join links are only recognised on known
//! conferencing hosts (see [`find_join_url`]).

use std::collections::{HashMap, HashSet};

use crate::civil::{
    civil_from_days, days_from_civil, days_in_month, unix_from_civil, weekday_from_days,
};

const DAY: i64 = 86_400;
/// Bounds on the work done for one feed.
const MAX_PERIODS: i64 = 20_000;
const MAX_PER_EVENT: usize = 800;
const MAX_EVENTS: usize = 20_000;

/// The user's local time zone, as the platform knows it (it has the real daylight-saving rules).
pub trait LocalZone {
    /// UTC instant (seconds since the epoch) for a local wall-clock time, given as seconds since the
    /// epoch *as if the wall clock were UTC*.
    fn local_to_utc(&self, wall: i64) -> i64;
    /// Local wall-clock seconds (same convention) for a UTC instant.
    fn utc_to_local(&self, utc: i64) -> i64;
}

/// A constant offset from UTC, in seconds (tests, and a fallback).
#[derive(Clone, Copy, Debug)]
pub struct FixedZone(pub i32);

impl LocalZone for FixedZone {
    fn local_to_utc(&self, wall: i64) -> i64 {
        wall - i64::from(self.0)
    }
    fn utc_to_local(&self, utc: i64) -> i64 {
        utc + i64::from(self.0)
    }
}

// ----- values ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum ZoneRef {
    Utc,
    Floating,
    Tz(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum When {
    /// An all-day date (days since 1970-01-01).
    Date(i64),
    /// A wall-clock time (seconds since the epoch as if UTC) in `zone`.
    Time { wall: i64, zone: ZoneRef },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Clone, Debug)]
struct Rrule {
    freq: Freq,
    interval: i64,
    count: Option<u32>,
    until: Option<When>,
    /// `(ordinal, weekday)`; ordinal 0 = every such weekday. Weekday 0 = Monday.
    by_day: Vec<(i32, u32)>,
    by_monthday: Vec<i32>,
    by_month: Vec<u32>,
    by_setpos: Vec<i32>,
    wkst: u32,
    /// Parts this reader does not implement: the series is then just its first occurrence.
    unsupported: bool,
}

#[derive(Clone, Debug, Default)]
struct Vevent {
    uid: String,
    start: Option<When>,
    end: Option<When>,
    duration: Option<i64>,
    summary: String,
    location: String,
    description: String,
    url: String,
    conference: Vec<String>,
    cancelled: bool,
    rrule: Option<Rrule>,
    rdates: Vec<When>,
    exdates: Vec<When>,
    recurrence_id: Option<When>,
}

// ----- time zones -----------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct YearlyRule {
    start_year: i32,
    month: u32,
    /// `(ordinal, weekday)` such as the last Sunday, or a day of the month.
    day: DayRule,
    tod: i64,
    until: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
enum DayRule {
    Nth { n: i32, weekday: u32 },
    MonthDay(i32),
}

#[derive(Clone, Debug)]
struct Observance {
    /// Offset before / after the transition, seconds east of UTC.
    from: i32,
    to: i32,
    once: Vec<i64>,
    yearly: Option<YearlyRule>,
}

/// The rules of one `VTIMEZONE`.
#[derive(Clone, Debug, Default)]
pub struct TzRules {
    observances: Vec<Observance>,
}

/// Day (days since the epoch) of the `n`-th (`n < 0`: from the end) `weekday` of a month.
fn nth_weekday(y: i32, m: u32, n: i32, weekday: u32) -> Option<i64> {
    let first = days_from_civil(y, m, 1);
    let dim = i64::from(days_in_month(y, m));
    if n > 0 {
        let offset = i64::from((weekday + 7 - weekday_from_days(first)) % 7);
        let d = offset + 7 * i64::from(n - 1);
        (d < dim).then_some(first + d)
    } else if n < 0 {
        let last = first + dim - 1;
        let offset = i64::from((weekday_from_days(last) + 7 - weekday) % 7);
        let d = offset + 7 * i64::from(-n - 1);
        (d < dim).then_some(last - d)
    } else {
        None
    }
}

/// Day for a month-day rule (`-1` = last day); `None` when the month has no such day.
fn month_day(y: i32, m: u32, d: i32) -> Option<i64> {
    let dim = days_in_month(y, m) as i32;
    let day = if d > 0 { d } else { dim + 1 + d };
    (1..=dim)
        .contains(&day)
        .then(|| days_from_civil(y, m, day as u32))
}

impl YearlyRule {
    fn onset_in_year(&self, y: i32) -> Option<i64> {
        let day = match self.day {
            DayRule::Nth { n, weekday } => nth_weekday(y, self.month, n, weekday)?,
            DayRule::MonthDay(d) => month_day(y, self.month, d)?,
        };
        Some(day * DAY + self.tod)
    }
}

impl Observance {
    /// The latest transition into this observance that is not after `wall`.
    fn latest_onset(&self, wall: i64) -> Option<i64> {
        let mut best: Option<i64> = None;
        let mut take = |t: i64| {
            if t <= wall {
                best = Some(best.map_or(t, |b| b.max(t)));
            }
        };
        for t in &self.once {
            take(*t);
        }
        if let Some(r) = &self.yearly {
            let y = civil_from_days(wall.div_euclid(DAY)).0;
            for y in [y - 1, y] {
                if y >= r.start_year
                    && let Some(t) = r.onset_in_year(y)
                    && r.until.is_none_or(|u| t <= u)
                {
                    take(t);
                }
            }
        }
        best
    }
}

impl TzRules {
    /// Seconds east of UTC in force at a local wall-clock time.
    pub fn offset_at_wall(&self, wall: i64) -> i32 {
        let mut best: Option<(i64, i32)> = None;
        for o in &self.observances {
            if let Some(t) = o.latest_onset(wall)
                && best.is_none_or(|(bt, _)| t > bt)
            {
                best = Some((t, o.to));
            }
        }
        best.map(|b| b.1)
            .or_else(|| self.observances.first().map(|o| o.from))
            .unwrap_or(0)
    }
}

// ----- the parsed calendar ----------------------------------------------------------------------------

/// A parsed feed. Build with [`Calendar::parse`], query with [`Calendar::occurrences`].
#[derive(Clone, Debug, Default)]
pub struct Calendar {
    events: Vec<Vevent>,
    zones: HashMap<String, TzRules>,
}

/// One concrete event in a time window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub uid: String,
    pub title: String,
    pub location: String,
    pub start_utc: i64,
    pub end_utc: i64,
    /// Local wall-clock seconds (as if UTC) of the start and end; all-day events are local midnights.
    pub start_local: i64,
    pub end_local: i64,
    pub all_day: bool,
    pub join_url: Option<String>,
}

struct Line {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.starts_with([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(&raw[1..]);
        } else {
            lines.push(raw.to_string());
        }
    }
    lines
}

/// Split at `sep` outside double quotes.
fn split_unquoted(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == sep && !quoted {
            out.push(&s[start..i]);
            start = i + c.len_utf8();
        }
    }
    out.push(&s[start..]);
    out
}

fn parse_line(l: &str) -> Option<Line> {
    let mut quoted = false;
    let mut colon = None;
    for (i, c) in l.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ':' if !quoted => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&l[..colon], &l[colon + 1..]);
    let mut parts = split_unquoted(head, ';').into_iter();
    let mut name = parts.next()?.trim().to_ascii_uppercase();
    // vCard-style group prefix ("item1.URL").
    if let Some(dot) = name.rfind('.') {
        name = name[dot + 1..].to_string();
    }
    let params = parts
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((
                k.trim().to_ascii_uppercase(),
                v.trim().trim_matches('"').to_string(),
            ))
        })
        .collect();
    Some(Line {
        name,
        params,
        value: value.to_string(),
    })
}

fn param<'a>(l: &'a Line, key: &str) -> Option<&'a str> {
    l.params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Undo RFC 5545 text escaping.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

fn digits(s: &str, from: usize, n: usize) -> Option<u32> {
    let part = s.get(from..from + n)?;
    if !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

fn parse_date(s: &str) -> Option<i64> {
    let (y, m, d) = (digits(s, 0, 4)?, digits(s, 4, 2)?, digits(s, 6, 2)?);
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y as i32, m) {
        return None;
    }
    Some(days_from_civil(y as i32, m, d))
}

/// `YYYYMMDDTHHMMSS[Z]` -> (wall seconds, is UTC).
fn parse_datetime(s: &str) -> Option<(i64, bool)> {
    let days = parse_date(s)?;
    if s.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    let (h, mi, sec) = (
        digits(s, 9, 2)?,
        digits(s, 11, 2)?,
        digits(s, 13, 2).unwrap_or(0),
    );
    if h > 24 || mi > 59 {
        return None;
    }
    let wall = days * DAY + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(sec.min(59));
    Some((wall, s.ends_with(['Z', 'z'])))
}

fn parse_when(l: &Line) -> Option<When> {
    let v = l.value.trim();
    if param(l, "VALUE").is_some_and(|x| x.eq_ignore_ascii_case("DATE")) || v.len() == 8 {
        return parse_date(v).map(When::Date);
    }
    let (wall, utc) = parse_datetime(v)?;
    let zone = if utc {
        ZoneRef::Utc
    } else if let Some(id) = param(l, "TZID") {
        ZoneRef::Tz(id.to_string())
    } else {
        ZoneRef::Floating
    };
    Some(When::Time { wall, zone })
}

fn parse_when_list(l: &Line) -> Vec<When> {
    l.value
        .split(',')
        .filter_map(|v| {
            parse_when(&Line {
                name: l.name.clone(),
                params: l.params.clone(),
                value: v.to_string(),
            })
        })
        .collect()
}

/// `P1DT2H30M`, `PT45M`, `P2W`.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let s = s.strip_prefix('P')?;
    let (mut total, mut num, mut in_time) = (0i64, String::new(), false);
    for c in s.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total = total.checked_add(match (c, in_time) {
                    ('W', _) => n.checked_mul(7 * DAY)?,
                    ('D', _) => n.checked_mul(DAY)?,
                    ('H', _) => n.checked_mul(3600)?,
                    ('M', true) => n.checked_mul(60)?,
                    ('S', _) => n,
                    _ => return None,
                })?;
            }
            _ => return None,
        }
    }
    Some(if neg { -total } else { total })
}

/// `+0100` / `-0530` / `+013000` -> seconds.
fn parse_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = (digits(s, 1, 2)? as i32, digits(s, 3, 2)? as i32);
    let sec = digits(s, 5, 2).unwrap_or(0) as i32;
    Some(sign * (h * 3600 + m * 60 + sec))
}

fn weekday_code(s: &str) -> Option<u32> {
    Some(match s {
        "MO" => 0,
        "TU" => 1,
        "WE" => 2,
        "TH" => 3,
        "FR" => 4,
        "SA" => 5,
        "SU" => 6,
        _ => return None,
    })
}

fn parse_rrule(value: &str, line: &Line) -> Option<Rrule> {
    let mut r = Rrule {
        freq: Freq::Daily,
        interval: 1,
        count: None,
        until: None,
        by_day: Vec::new(),
        by_monthday: Vec::new(),
        by_month: Vec::new(),
        by_setpos: Vec::new(),
        wkst: 0,
        unsupported: false,
    };
    let mut have_freq = false;
    for part in value.split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                have_freq = true;
                match v.to_ascii_uppercase().as_str() {
                    "DAILY" => r.freq = Freq::Daily,
                    "WEEKLY" => r.freq = Freq::Weekly,
                    "MONTHLY" => r.freq = Freq::Monthly,
                    "YEARLY" => r.freq = Freq::Yearly,
                    _ => r.unsupported = true,
                }
            }
            "INTERVAL" => r.interval = v.parse::<i64>().ok()?.clamp(1, 1000),
            "COUNT" => r.count = Some(v.parse::<u32>().ok()?.min(100_000)),
            "UNTIL" => {
                r.until = parse_when(&Line {
                    name: line.name.clone(),
                    params: Vec::new(),
                    value: v.to_string(),
                });
            }
            "BYDAY" => {
                for d in v.split(',') {
                    let d = d.trim().to_ascii_uppercase();
                    if !d.is_ascii() {
                        return None;
                    }
                    let split = d.len().saturating_sub(2);
                    let (ord, wd) = d.split_at(split);
                    let wd = weekday_code(wd)?;
                    let ord = if ord.is_empty() {
                        0
                    } else {
                        ord.parse::<i32>().ok()?
                    };
                    r.by_day.push((ord, wd));
                }
            }
            "BYMONTHDAY" => {
                r.by_monthday = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            }
            "BYMONTH" => {
                r.by_month = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            }
            "BYSETPOS" => {
                r.by_setpos = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            }
            "WKST" => r.wkst = weekday_code(&v.to_ascii_uppercase()).unwrap_or(0),
            "BYWEEKNO" | "BYYEARDAY" | "BYHOUR" | "BYMINUTE" | "BYSECOND" => {
                r.unsupported = true;
            }
            _ => {}
        }
    }
    have_freq.then_some(r)
}

impl Calendar {
    /// Read a feed. Never fails: whatever cannot be understood is skipped.
    pub fn parse(text: &str) -> Calendar {
        let mut cal = Calendar::default();
        let mut stack: Vec<String> = Vec::new();
        let mut ev: Option<Vevent> = None;
        let mut tz: Option<(String, Vec<Observance>)> = None;
        let mut obs: Option<Observance> = None;
        let mut obs_start: Option<i64> = None;
        let mut obs_rrule: Option<Rrule> = None;

        for raw in unfold(text) {
            let Some(l) = parse_line(&raw) else { continue };
            match l.name.as_str() {
                "BEGIN" => {
                    let what = l.value.trim().to_ascii_uppercase();
                    match what.as_str() {
                        "VEVENT" if ev.is_none() => ev = Some(Vevent::default()),
                        "VTIMEZONE" => tz = Some((String::new(), Vec::new())),
                        "STANDARD" | "DAYLIGHT" if tz.is_some() => {
                            obs = Some(Observance {
                                from: 0,
                                to: 0,
                                once: Vec::new(),
                                yearly: None,
                            });
                            obs_start = None;
                            obs_rrule = None;
                        }
                        _ => {}
                    }
                    stack.push(what);
                }
                "END" => {
                    let what = l.value.trim().to_ascii_uppercase();
                    // Tolerate truncated/misnested input: pop back to the matching BEGIN.
                    if let Some(pos) = stack.iter().rposition(|s| *s == what) {
                        stack.truncate(pos);
                    }
                    match what.as_str() {
                        "VEVENT" => {
                            if let Some(e) = ev.take()
                                && cal.events.len() < MAX_EVENTS
                            {
                                cal.events.push(e);
                            }
                        }
                        "STANDARD" | "DAYLIGHT" => {
                            if let (Some(mut o), Some((_, list))) = (obs.take(), tz.as_mut()) {
                                if let Some(start) = obs_start {
                                    o.once.push(start);
                                    o.yearly =
                                        obs_rrule.take().and_then(|r| yearly_rule(&r, start));
                                }
                                list.push(o);
                            }
                        }
                        "VTIMEZONE" => {
                            if let Some((id, list)) = tz.take()
                                && !id.is_empty()
                                && !list.is_empty()
                            {
                                cal.zones.insert(id, TzRules { observances: list });
                            }
                        }
                        _ => {}
                    }
                }
                _ => {
                    let top = stack.last().map(String::as_str);
                    match top {
                        Some("VEVENT") => {
                            if let Some(e) = ev.as_mut() {
                                apply_event_prop(e, &l);
                            }
                        }
                        Some("VTIMEZONE") => {
                            if l.name == "TZID"
                                && let Some((id, _)) = tz.as_mut()
                            {
                                *id = l.value.trim().to_string();
                            }
                        }
                        Some("STANDARD" | "DAYLIGHT") => {
                            if let Some(o) = obs.as_mut() {
                                match l.name.as_str() {
                                    "TZOFFSETFROM" => o.from = parse_offset(&l.value).unwrap_or(0),
                                    "TZOFFSETTO" => o.to = parse_offset(&l.value).unwrap_or(0),
                                    "DTSTART" => {
                                        obs_start = parse_datetime(l.value.trim()).map(|d| d.0);
                                    }
                                    "RDATE" => {
                                        for w in parse_when_list(&l) {
                                            if let When::Time { wall, .. } = w {
                                                o.once.push(wall);
                                            }
                                        }
                                    }
                                    "RRULE" => obs_rrule = parse_rrule(&l.value, &l),
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        cal
    }

    /// Number of events (series count once). For diagnostics.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// A yearly rule from a VTIMEZONE observance's RRULE (`FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU`).
fn yearly_rule(r: &Rrule, start_wall: i64) -> Option<YearlyRule> {
    if r.freq != Freq::Yearly || r.unsupported {
        return None;
    }
    let (y, m, d) = civil_from_days(start_wall.div_euclid(DAY));
    let month = r.by_month.first().copied().unwrap_or(m);
    let day = if let Some(&(n, wd)) = r.by_day.first() {
        DayRule::Nth {
            n: if n == 0 { 1 } else { n },
            weekday: wd,
        }
    } else if let Some(&md) = r.by_monthday.first() {
        DayRule::MonthDay(md)
    } else {
        DayRule::MonthDay(d as i32)
    };
    let until = match &r.until {
        Some(When::Time { wall, .. }) => Some(*wall),
        Some(When::Date(d)) => Some(*d * DAY + DAY - 1),
        None => None,
    };
    Some(YearlyRule {
        start_year: y,
        month,
        day,
        tod: start_wall.rem_euclid(DAY),
        until,
    })
}

fn apply_event_prop(e: &mut Vevent, l: &Line) {
    match l.name.as_str() {
        "UID" => e.uid = l.value.trim().to_string(),
        "DTSTART" => e.start = parse_when(l),
        "DTEND" => e.end = parse_when(l),
        "DURATION" => e.duration = parse_duration(&l.value),
        "SUMMARY" => e.summary = unescape(&l.value),
        "LOCATION" => e.location = unescape(&l.value),
        "DESCRIPTION" => e.description = unescape(&l.value),
        "URL" => e.url = l.value.trim().to_string(),
        "STATUS" => e.cancelled = l.value.trim().eq_ignore_ascii_case("CANCELLED"),
        "RRULE" => e.rrule = parse_rrule(&l.value, l),
        "RDATE" => e.rdates.extend(parse_when_list(l)),
        "EXDATE" => e.exdates.extend(parse_when_list(l)),
        "RECURRENCE-ID" => e.recurrence_id = parse_when(l),
        "X-GOOGLE-CONFERENCE"
        | "X-MICROSOFT-SKYPETEAMSMEETINGURL"
        | "X-MICROSOFT-ONLINEMEETINGEXTERNALLINK"
        | "X-MICROSOFT-ONLINEMEETINGCONFLINK" => e.conference.push(l.value.trim().to_string()),
        _ => {}
    }
}

// ----- occurrences ------------------------------------------------------------------------------------

/// What identifies one instance of a series (to match EXDATE / RECURRENCE-ID): the UTC start of a
/// timed instance, or the date of an all-day one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Slot {
    Timed(i64),
    Day(i64),
}

struct Resolver<'a> {
    cal: &'a Calendar,
    local: &'a dyn LocalZone,
}

impl Resolver<'_> {
    fn wall_to_utc(&self, wall: i64, zone: &ZoneRef) -> i64 {
        match zone {
            ZoneRef::Utc => wall,
            ZoneRef::Floating => self.local.local_to_utc(wall),
            ZoneRef::Tz(id) => match self.cal.zones.get(id) {
                Some(rules) => wall - i64::from(rules.offset_at_wall(wall)),
                None => self.local.local_to_utc(wall),
            },
        }
    }

    fn slot(&self, w: &When) -> Slot {
        match w {
            When::Date(d) => Slot::Day(*d),
            When::Time { wall, zone } => Slot::Timed(self.wall_to_utc(*wall, zone)),
        }
    }
}

/// Dates (days since the epoch) of period `k` of a recurrence.
fn period_dates(r: &Rrule, start_days: i64, k: i64) -> Vec<i64> {
    let (sy, sm, sd) = civil_from_days(start_days);
    let in_months = |m: u32| r.by_month.is_empty() || r.by_month.contains(&m);
    let by_day_matches = |day: i64| {
        r.by_day.is_empty() || {
            let wd = weekday_from_days(day);
            r.by_day.iter().any(|&(_, w)| w == wd)
        }
    };
    let mut out: Vec<i64> = Vec::new();
    // All dates of month (y, m) selected by the rule (BYMONTHDAY / BYDAY / the start's day).
    let month_set = |y: i32, m: u32| -> Vec<i64> {
        let mut v = Vec::new();
        if !r.by_monthday.is_empty() {
            for &d in &r.by_monthday {
                if let Some(day) = month_day(y, m, d)
                    && by_day_matches(day)
                {
                    v.push(day);
                }
            }
        } else if !r.by_day.is_empty() {
            for &(ord, wd) in &r.by_day {
                if ord != 0 {
                    v.extend(nth_weekday(y, m, ord, wd));
                } else {
                    let first = days_from_civil(y, m, 1);
                    for d in 0..i64::from(days_in_month(y, m)) {
                        if weekday_from_days(first + d) == wd {
                            v.push(first + d);
                        }
                    }
                }
            }
        } else {
            v.extend(month_day(y, m, sd as i32));
        }
        v.sort_unstable();
        v.dedup();
        v
    };
    match r.freq {
        Freq::Daily => {
            let day = start_days + k * r.interval;
            let (y, m, _) = civil_from_days(day);
            let md_ok = r.by_monthday.is_empty()
                || r.by_monthday
                    .iter()
                    .any(|&x| month_day(y, m, x) == Some(day));
            if in_months(m) && md_ok && by_day_matches(day) {
                out.push(day);
            }
        }
        Freq::Weekly => {
            let wk0 = start_days - i64::from((weekday_from_days(start_days) + 7 - r.wkst) % 7);
            let base = wk0 + 7 * r.interval * k;
            let days: Vec<u32> = if r.by_day.is_empty() {
                vec![weekday_from_days(start_days)]
            } else {
                r.by_day.iter().map(|&(_, w)| w).collect()
            };
            for wd in days {
                let day = base + i64::from((wd + 7 - r.wkst) % 7);
                if in_months(civil_from_days(day).1) {
                    out.push(day);
                }
            }
        }
        Freq::Monthly => {
            let idx = i64::from(sy) * 12 + i64::from(sm) - 1 + k * r.interval;
            let (y, m) = ((idx.div_euclid(12)) as i32, (idx.rem_euclid(12) + 1) as u32);
            if in_months(m) {
                out = month_set(y, m);
            }
        }
        Freq::Yearly => {
            let y = sy + (k * r.interval) as i32;
            let months: Vec<u32> = if r.by_month.is_empty() {
                vec![sm]
            } else {
                r.by_month.clone()
            };
            for m in months {
                out.extend(month_set(y, m));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    if !r.by_setpos.is_empty() {
        let n = out.len() as i32;
        let mut picked: Vec<i64> = r
            .by_setpos
            .iter()
            .filter_map(|&p| {
                let i = if p > 0 { p - 1 } else { n + p };
                (0..n).contains(&i).then(|| out[i as usize])
            })
            .collect();
        picked.sort_unstable();
        picked.dedup();
        out = picked;
    }
    out
}

/// First day of period `k`, to know when the iteration has passed the window.
fn period_first_day(r: &Rrule, start_days: i64, k: i64) -> i64 {
    let (sy, sm, _) = civil_from_days(start_days);
    match r.freq {
        Freq::Daily => start_days + k * r.interval,
        Freq::Weekly => {
            let wk0 = start_days - i64::from((weekday_from_days(start_days) + 7 - r.wkst) % 7);
            wk0 + 7 * r.interval * k
        }
        Freq::Monthly => {
            let idx = i64::from(sy) * 12 + i64::from(sm) - 1 + k * r.interval;
            days_from_civil(
                (idx.div_euclid(12)) as i32,
                (idx.rem_euclid(12) + 1) as u32,
                1,
            )
        }
        Freq::Yearly => days_from_civil(sy + (k * r.interval) as i32, 1, 1),
    }
}

/// A first period index from which to start so that no occurrence in the window is missed.
fn skip_ahead(r: &Rrule, start_days: i64, window_start_days: i64) -> i64 {
    let gap = window_start_days - start_days;
    if gap <= 0 {
        return 0;
    }
    let k = match r.freq {
        Freq::Daily => gap / r.interval,
        Freq::Weekly => gap / (7 * r.interval),
        // Divide by the *longest* period so the estimate never overshoots the window.
        Freq::Monthly => gap / (31 * r.interval),
        Freq::Yearly => gap / (366 * r.interval),
    };
    (k - 1).max(0)
}

impl Calendar {
    /// Every event instance overlapping `[from_utc, to_utc)`, sorted by start, at most `cap`.
    pub fn occurrences(
        &self,
        from_utc: i64,
        to_utc: i64,
        local: &dyn LocalZone,
        cap: usize,
    ) -> Vec<Occurrence> {
        let res = Resolver { cal: self, local };
        // Overrides replace one instance of a series: (uid, original slot) -> the override. A cancelled
        // override is in the map too: it removes the generated instance and is itself skipped below.
        let mut overrides: HashSet<(&str, Slot)> = HashSet::new();
        for e in &self.events {
            if let Some(rid) = &e.recurrence_id {
                overrides.insert((e.uid.as_str(), res.slot(rid)));
            }
        }
        let mut out: Vec<Occurrence> = Vec::new();
        let overlaps = |s: i64, e: i64| {
            if e > s {
                e > from_utc && s < to_utc
            } else {
                s >= from_utc && s < to_utc
            }
        };
        for ev in &self.events {
            if ev.cancelled || out.len() >= cap {
                continue;
            }
            let Some(start) = &ev.start else { continue };
            let Some(base) = self.span_of(ev, &res) else {
                continue;
            };
            let join = find_join_url_in(ev);
            let emit = |out: &mut Vec<Occurrence>, s: (i64, i64, i64, i64, bool)| {
                if out.len() < cap && overlaps(s.0, s.1) {
                    out.push(Occurrence {
                        uid: ev.uid.clone(),
                        title: ev.summary.clone(),
                        location: ev.location.clone(),
                        start_utc: s.0,
                        end_utc: s.1,
                        start_local: s.2,
                        end_local: s.3,
                        all_day: s.4,
                        join_url: join.clone(),
                    });
                    true
                } else {
                    false
                }
            };
            // An override is an event of its own (it may have moved into or out of the window).
            if ev.recurrence_id.is_some() {
                emit(&mut out, base);
                continue;
            }
            let (dur_utc, dur_days) = (base.1 - base.0, (base.3 - base.2) / DAY);
            let excluded: HashSet<Slot> = ev.exdates.iter().map(|w| res.slot(w)).collect();
            // The candidate instances: the first one, RDATEs, and the RRULE expansion.
            let mut starts: Vec<When> = vec![start.clone()];
            starts.extend(ev.rdates.iter().cloned());
            if let Some(rule) = &ev.rrule
                && !rule.unsupported
            {
                starts.extend(self.expand_rule(rule, start, from_utc, to_utc, &res));
            }
            let mut seen: HashSet<Slot> = HashSet::new();
            let mut emitted = 0usize;
            for w in starts {
                let slot = res.slot(&w);
                if !seen.insert(slot)
                    || excluded.contains(&slot)
                    || overrides.contains(&(ev.uid.as_str(), slot))
                {
                    continue;
                }
                let span = match &w {
                    When::Date(d) => {
                        let (sl, el) = (*d * DAY, (*d + dur_days.max(1)) * DAY);
                        (local.local_to_utc(sl), local.local_to_utc(el), sl, el, true)
                    }
                    When::Time { wall, zone } => {
                        let s = res.wall_to_utc(*wall, zone);
                        let e = s + dur_utc;
                        (s, e, local.utc_to_local(s), local.utc_to_local(e), false)
                    }
                };
                if emit(&mut out, span) {
                    emitted += 1;
                    if emitted >= MAX_PER_EVENT {
                        break;
                    }
                }
            }
        }
        out.sort_by(|a, b| (a.start_utc, &a.title).cmp(&(b.start_utc, &b.title)));
        out
    }

    /// Start/end/local start/local end/all-day of an event's first instance.
    fn span_of(&self, ev: &Vevent, res: &Resolver) -> Option<(i64, i64, i64, i64, bool)> {
        let local = res.local;
        match ev.start.as_ref()? {
            When::Date(d) => {
                let end_days = match &ev.end {
                    Some(When::Date(e)) if *e > *d => *e,
                    _ => match ev.duration {
                        Some(secs) if secs >= DAY => *d + secs / DAY,
                        _ => *d + 1,
                    },
                };
                let (sl, el) = (*d * DAY, end_days * DAY);
                Some((local.local_to_utc(sl), local.local_to_utc(el), sl, el, true))
            }
            When::Time { wall, zone } => {
                let s = res.wall_to_utc(*wall, zone);
                let e = match (&ev.end, ev.duration) {
                    (Some(When::Time { wall: w2, zone: z2 }), _) => res.wall_to_utc(*w2, z2),
                    (Some(When::Date(d)), _) => local.local_to_utc(*d * DAY),
                    (None, Some(secs)) => s + secs,
                    (None, None) => s,
                };
                // A negative or absurd length is treated as an instant.
                let e = if e < s || e - s > 366 * DAY { s } else { e };
                Some((s, e, local.utc_to_local(s), local.utc_to_local(e), false))
            }
        }
    }

    /// Instances of a recurrence rule that can fall in the window, as `When`s (the first instance
    /// itself is included by the caller).
    fn expand_rule(
        &self,
        rule: &Rrule,
        start: &When,
        from_utc: i64,
        to_utc: i64,
        res: &Resolver,
    ) -> Vec<When> {
        let (start_days, tod, zone) = match start {
            When::Date(d) => (*d, None, None),
            When::Time { wall, zone } => {
                (wall.div_euclid(DAY), Some(wall.rem_euclid(DAY)), Some(zone))
            }
        };
        // Window in days, padded generously for zone offsets (the exact test happens later).
        let win_start = from_utc.div_euclid(DAY) - 2;
        let win_end = to_utc.div_euclid(DAY) + 2;
        let until_ok = |day: i64, wall: i64| -> bool {
            match &rule.until {
                None => true,
                Some(When::Date(u)) => day <= *u,
                Some(When::Time { wall: u, zone: uz }) => match (zone, uz) {
                    // The rule's UNTIL is UTC: compare instants.
                    (Some(z), ZoneRef::Utc) => res.wall_to_utc(wall, z) <= *u,
                    _ => wall <= *u,
                },
            }
        };
        let mut out = Vec::new();
        let mut counted = 0u32;
        let mut k = if rule.count.is_some() {
            0
        } else {
            skip_ahead(rule, start_days, win_start)
        };
        let mut periods = 0i64;
        'outer: loop {
            periods += 1;
            if periods > MAX_PERIODS || out.len() >= MAX_PER_EVENT {
                break;
            }
            if period_first_day(rule, start_days, k) > win_end {
                break;
            }
            for day in period_dates(rule, start_days, k) {
                let wall = day * DAY + tod.unwrap_or(0);
                if day < start_days || (day == start_days && wall < start_wall(start)) {
                    continue;
                }
                if !until_ok(day, wall) {
                    break 'outer;
                }
                counted += 1;
                if rule.count.is_some_and(|c| counted > c) {
                    break 'outer;
                }
                if day >= win_start - 1 && day <= win_end {
                    out.push(match zone {
                        Some(z) => When::Time {
                            wall,
                            zone: z.clone(),
                        },
                        None => When::Date(day),
                    });
                }
            }
            k += 1;
        }
        out
    }
}

fn start_wall(w: &When) -> i64 {
    match w {
        When::Date(d) => *d * DAY,
        When::Time { wall, .. } => *wall,
    }
}

// ----- join links -------------------------------------------------------------------------------------

/// Hosts a "Join" button may open. A calendar feed contains text written by other people, so only
/// well-known conferencing services qualify; anything else is never offered as a one-click action.
const JOIN_HOSTS: &[&str] = &[
    "meet.google.com",
    "zoom.us",
    "zoomgov.com",
    "teams.microsoft.com",
    "teams.live.com",
    "webex.com",
    "whereby.com",
    "meet.jit.si",
    "gotomeeting.com",
    "gotomeet.me",
    "bluejeans.com",
    "chime.aws",
    "around.co",
    "meet.goto.com",
];

fn host_allowed(host: &str) -> bool {
    JOIN_HOSTS
        .iter()
        .any(|h| host == *h || host.strip_suffix(h).is_some_and(|p| p.ends_with('.')))
}

/// The first `https://` link in `text` that points at a conferencing service.
pub fn find_join_url(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("https://") {
        let start = from + pos;
        from = start + 8;
        let rest = &text[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '\\' | '`'))
            .unwrap_or(rest.len());
        let url = rest[..end].trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '!', '?']);
        let authority = url[8..].split(['/', '?', '#']).next().unwrap_or("");
        // `https://meet.google.com@evil.example/` has user-info: refuse anything with '@' or a port.
        if authority.is_empty() || authority.contains(['@', ':']) {
            continue;
        }
        if host_allowed(&authority.to_ascii_lowercase()) && url.len() <= 2048 {
            return Some(url.to_string());
        }
    }
    None
}

fn find_join_url_in(ev: &Vevent) -> Option<String> {
    ev.conference
        .iter()
        .chain(std::iter::once(&ev.url))
        .chain(std::iter::once(&ev.location))
        .chain(std::iter::once(&ev.description))
        .find_map(|t| find_join_url(t))
}

/// Seconds since the epoch of a UTC civil time (a convenience for tests and callers).
pub fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    unix_from_civil(y, mo, d, h, mi, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z0: FixedZone = FixedZone(0);

    fn feed(body: &str) -> Calendar {
        Calendar::parse(&format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}\r\nEND:VCALENDAR\r\n"
        ))
    }

    fn vevent(props: &str) -> String {
        format!("BEGIN:VEVENT\r\nUID:u1\r\n{props}\r\nEND:VEVENT")
    }

    fn day(y: i32, m: u32, d: u32) -> i64 {
        utc(y, m, d, 0, 0)
    }

    fn starts(o: &[Occurrence]) -> Vec<i64> {
        o.iter().map(|x| x.start_utc).collect()
    }

    const LONDON: &str = "BEGIN:VTIMEZONE\r\nTZID:Europe/London\r\n\
        BEGIN:DAYLIGHT\r\nTZOFFSETFROM:+0000\r\nTZOFFSETTO:+0100\r\nTZNAME:BST\r\nDTSTART:19700329T010000\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nEND:DAYLIGHT\r\n\
        BEGIN:STANDARD\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0000\r\nTZNAME:GMT\r\nDTSTART:19701025T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nEND:STANDARD\r\n\
        END:VTIMEZONE\r\n";

    #[test]
    fn a_single_utc_event_with_escapes_and_folding() {
        let cal = feed(&vevent(
            "DTSTART:20261006T130000Z\r\nDTEND:20261006T140000Z\r\nSUMMARY:Design\\, review\r\n  with Sam\r\nLOCATION:Room 4\\; floor 2",
        ));
        let o = cal.occurrences(day(2026, 10, 6), day(2026, 10, 7), &Z0, 100);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].title, "Design, review with Sam");
        assert_eq!(o[0].location, "Room 4; floor 2");
        assert_eq!(
            (o[0].start_utc, o[0].end_utc),
            (utc(2026, 10, 6, 13, 0), utc(2026, 10, 6, 14, 0))
        );
        assert!(!o[0].all_day);
    }

    #[test]
    fn local_times_follow_the_platform_zone() {
        let cal = feed(&vevent(
            "DTSTART:20261006T130000Z\r\nDTEND:20261006T140000Z\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 6), day(2026, 10, 7), &FixedZone(2 * 3600), 10);
        assert_eq!(o[0].start_local, utc(2026, 10, 6, 15, 0));
        assert_eq!(o[0].end_local, utc(2026, 10, 6, 16, 0));
    }

    #[test]
    fn floating_times_are_local_times() {
        let cal = feed(&vevent(
            "DTSTART:20261006T090000\r\nDTEND:20261006T093000\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(
            day(2026, 10, 5),
            day(2026, 10, 8),
            &FixedZone(-4 * 3600),
            10,
        );
        assert_eq!(o[0].start_utc, utc(2026, 10, 6, 13, 0), "09:00 at UTC-4");
        assert_eq!(o[0].start_local, utc(2026, 10, 6, 9, 0));
    }

    #[test]
    fn all_day_events_span_whole_local_days() {
        let cal = feed(&vevent(
            "DTSTART;VALUE=DATE:20261010\r\nDTEND;VALUE=DATE:20261012\r\nSUMMARY:Trip",
        ));
        let o = cal.occurrences(day(2026, 10, 1), day(2026, 11, 1), &FixedZone(3600), 10);
        assert_eq!(o.len(), 1);
        assert!(o[0].all_day);
        assert_eq!(o[0].start_local, day(2026, 10, 10));
        assert_eq!(
            o[0].end_local,
            day(2026, 10, 12),
            "DTEND is exclusive: two days"
        );
        assert_eq!(o[0].start_utc, day(2026, 10, 10) - 3600);
        let single = feed(&vevent("DTSTART;VALUE=DATE:20261010\r\nSUMMARY:Day"));
        let o = single.occurrences(day(2026, 10, 1), day(2026, 11, 1), &Z0, 10);
        assert_eq!(o[0].end_local - o[0].start_local, DAY, "no DTEND: one day");
    }

    #[test]
    fn tzid_times_use_the_feeds_vtimezone_rules() {
        let body = format!(
            "{LONDON}{}",
            vevent(
                "DTSTART;TZID=Europe/London:20261006T090000\r\nDTEND;TZID=Europe/London:20261006T100000\r\nSUMMARY:BST"
            )
        );
        let o = feed(&body).occurrences(day(2026, 10, 6), day(2026, 10, 7), &Z0, 10);
        assert_eq!(
            o[0].start_utc,
            utc(2026, 10, 6, 8, 0),
            "BST is UTC+1 in October"
        );
        let body = format!(
            "{LONDON}{}",
            vevent("DTSTART;TZID=Europe/London:20261201T090000\r\nSUMMARY:GMT")
        );
        let o = feed(&body).occurrences(day(2026, 12, 1), day(2026, 12, 2), &Z0, 10);
        assert_eq!(o[0].start_utc, utc(2026, 12, 1, 9, 0), "GMT in December");
    }

    #[test]
    fn dst_transition_days_are_exact() {
        // Last Sunday of March 2026 is the 29th: 01:00 UTC is when London springs forward.
        let tz = Calendar::parse(&format!("BEGIN:VCALENDAR\r\n{LONDON}END:VCALENDAR")).zones["Europe/London"].clone();
        let before = utc(2026, 3, 29, 0, 59);
        let after = utc(2026, 3, 29, 2, 0);
        assert_eq!(tz.offset_at_wall(before), 0);
        assert_eq!(tz.offset_at_wall(after), 3600);
        // Last Sunday of October 2026 is the 25th.
        assert_eq!(tz.offset_at_wall(utc(2026, 10, 25, 1, 59)), 3600);
        assert_eq!(tz.offset_at_wall(utc(2026, 10, 25, 3, 0)), 0);
    }

    #[test]
    fn us_style_nth_sunday_rules() {
        let us = "BEGIN:VTIMEZONE\r\nTZID:Eastern\r\n\
            BEGIN:DAYLIGHT\r\nTZOFFSETFROM:-0500\r\nTZOFFSETTO:-0400\r\nDTSTART:20070311T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=2SU\r\nEND:DAYLIGHT\r\n\
            BEGIN:STANDARD\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nDTSTART:20071104T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=1SU\r\nEND:STANDARD\r\n\
            END:VTIMEZONE\r\n";
        let tz = Calendar::parse(&format!("BEGIN:VCALENDAR\r\n{us}END:VCALENDAR")).zones["Eastern"]
            .clone();
        assert_eq!(
            tz.offset_at_wall(utc(2026, 3, 8, 3, 0)),
            -4 * 3600,
            "2026-03-08 is the second Sunday"
        );
        assert_eq!(tz.offset_at_wall(utc(2026, 3, 8, 1, 0)), -5 * 3600);
        assert_eq!(
            tz.offset_at_wall(utc(2026, 11, 1, 3, 0)),
            -5 * 3600,
            "2026-11-01 is the first Sunday"
        );
        assert_eq!(tz.offset_at_wall(utc(2026, 7, 4, 12, 0)), -4 * 3600);
    }

    #[test]
    fn an_unknown_tzid_falls_back_to_local_time() {
        let cal = feed(&vevent(
            "DTSTART;TZID=Mystery/Zone:20261006T090000\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 5), day(2026, 10, 8), &FixedZone(3600), 10);
        assert_eq!(o[0].start_utc, utc(2026, 10, 6, 8, 0));
    }

    #[test]
    fn daily_with_count_and_interval() {
        let cal = feed(&vevent(
            "DTSTART:20261001T090000Z\r\nDTEND:20261001T093000Z\r\nRRULE:FREQ=DAILY;INTERVAL=2;COUNT=4\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 9, 1), day(2026, 12, 1), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 10, 1, 9, 0),
                utc(2026, 10, 3, 9, 0),
                utc(2026, 10, 5, 9, 0),
                utc(2026, 10, 7, 9, 0)
            ]
        );
        assert!(o.iter().all(|x| x.end_utc - x.start_utc == 1800));
    }

    #[test]
    fn weekly_by_day_every_other_week() {
        // 2026-10-05 is a Monday.
        let cal = feed(&vevent(
            "DTSTART:20261005T100000Z\r\nRRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE,FR;COUNT=7\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 1), day(2026, 12, 31), &Z0, 100);
        let days: Vec<(u32, u32)> = o
            .iter()
            .map(|x| {
                let (_, m, d) = civil_from_days(x.start_utc.div_euclid(DAY));
                (m, d)
            })
            .collect();
        assert_eq!(
            days,
            vec![
                (10, 5),
                (10, 7),
                (10, 9),
                (10, 19),
                (10, 21),
                (10, 23),
                (11, 2)
            ]
        );
    }

    #[test]
    fn weekly_defaults_to_the_start_weekday() {
        let cal = feed(&vevent(
            "DTSTART:20261006T100000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 20), day(2026, 11, 4), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 10, 20, 10, 0),
                utc(2026, 10, 27, 10, 0),
                utc(2026, 11, 3, 10, 0)
            ]
        );
    }

    #[test]
    fn monthly_rules() {
        // Last day of every month.
        let cal = feed(&vevent(
            "DTSTART:20260131T120000Z\r\nRRULE:FREQ=MONTHLY;BYMONTHDAY=-1;COUNT=4\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 1, 1), day(2026, 12, 31), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 1, 31, 12, 0),
                utc(2026, 2, 28, 12, 0),
                utc(2026, 3, 31, 12, 0),
                utc(2026, 4, 30, 12, 0)
            ]
        );
        // Second Tuesday.
        let cal = feed(&vevent(
            "DTSTART:20261013T120000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=2TU;COUNT=3\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 1), day(2027, 3, 1), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 10, 13, 12, 0),
                utc(2026, 11, 10, 12, 0),
                utc(2026, 12, 8, 12, 0)
            ]
        );
        // Last Friday via BYSETPOS.
        let cal = feed(&vevent(
            "DTSTART:20261030T120000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=FR;BYSETPOS=-1;COUNT=2\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 1), day(2027, 3, 1), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![utc(2026, 10, 30, 12, 0), utc(2026, 11, 27, 12, 0)]
        );
        // The 31st skips months without one.
        let cal = feed(&vevent(
            "DTSTART:20260131T120000Z\r\nRRULE:FREQ=MONTHLY;COUNT=3\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 1, 1), day(2026, 12, 31), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 1, 31, 12, 0),
                utc(2026, 3, 31, 12, 0),
                utc(2026, 5, 31, 12, 0)
            ]
        );
    }

    #[test]
    fn yearly_rules_and_leap_days() {
        let cal = feed(&vevent(
            "DTSTART;VALUE=DATE:20260314\r\nRRULE:FREQ=YEARLY;COUNT=3\r\nSUMMARY:Pi day",
        ));
        let o = cal.occurrences(day(2026, 1, 1), day(2030, 1, 1), &Z0, 100);
        assert_eq!(o.len(), 3);
        assert!(o.iter().all(|x| x.all_day));
        let cal = feed(&vevent(
            "DTSTART:20240229T100000Z\r\nRRULE:FREQ=YEARLY;COUNT=3\r\nSUMMARY:Leap",
        ));
        let o = cal.occurrences(day(2024, 1, 1), day(2040, 1, 1), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2024, 2, 29, 10, 0),
                utc(2028, 2, 29, 10, 0),
                utc(2032, 2, 29, 10, 0)
            ],
            "Feb 29 only exists in leap years"
        );
    }

    #[test]
    fn until_ends_a_series_in_utc_and_by_date() {
        let cal = feed(&vevent(
            "DTSTART:20261001T090000Z\r\nRRULE:FREQ=DAILY;UNTIL=20261003T090000Z\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 9, 1), day(2026, 12, 1), &Z0, 100);
        assert_eq!(o.len(), 3, "UNTIL is inclusive");
        let cal = feed(&vevent(
            "DTSTART;VALUE=DATE:20261001\r\nRRULE:FREQ=DAILY;UNTIL=20261002\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 9, 1), day(2026, 12, 1), &Z0, 100);
        assert_eq!(o.len(), 2);
    }

    #[test]
    fn exdate_removes_one_instance() {
        let cal = feed(&vevent(
            "DTSTART:20261001T090000Z\r\nRRULE:FREQ=DAILY;COUNT=4\r\nEXDATE:20261002T090000Z\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 9, 1), day(2026, 12, 1), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 10, 1, 9, 0),
                utc(2026, 10, 3, 9, 0),
                utc(2026, 10, 4, 9, 0)
            ]
        );
    }

    #[test]
    fn rdate_adds_instances() {
        let cal = feed(&vevent(
            "DTSTART:20261001T090000Z\r\nRDATE:20261015T090000Z,20261020T090000Z\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 9, 1), day(2026, 12, 1), &Z0, 100);
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 10, 1, 9, 0),
                utc(2026, 10, 15, 9, 0),
                utc(2026, 10, 20, 9, 0)
            ]
        );
    }

    #[test]
    fn a_recurrence_override_moves_or_cancels_one_instance() {
        let series = "BEGIN:VEVENT\r\nUID:s\r\nDTSTART:20261005T090000Z\r\nDTEND:20261005T093000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n";
        let moved = "BEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID:20261006T090000Z\r\nDTSTART:20261006T150000Z\r\nDTEND:20261006T153000Z\r\nSUMMARY:Standup (late)\r\nEND:VEVENT\r\n";
        let cancelled = "BEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID:20261007T090000Z\r\nDTSTART:20261007T090000Z\r\nSTATUS:CANCELLED\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n";
        let cal = feed(&format!("{series}{moved}{cancelled}"));
        let o = cal.occurrences(day(2026, 10, 1), day(2026, 10, 31), &Z0, 100);
        let got: Vec<(i64, &str)> = o.iter().map(|x| (x.start_utc, x.title.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (utc(2026, 10, 5, 9, 0), "Standup"),
                (utc(2026, 10, 6, 15, 0), "Standup (late)"),
            ]
        );
    }

    #[test]
    fn cancelled_events_are_skipped() {
        let cal = feed(&vevent(
            "DTSTART:20261001T090000Z\r\nSTATUS:CANCELLED\r\nSUMMARY:x",
        ));
        assert!(
            cal.occurrences(day(2026, 9, 1), day(2026, 12, 1), &Z0, 100)
                .is_empty()
        );
    }

    #[test]
    fn a_series_in_a_zone_keeps_its_local_time_across_dst() {
        let body = format!(
            "{LONDON}{}",
            vevent(
                "DTSTART;TZID=Europe/London:20261019T090000\r\nDTEND;TZID=Europe/London:20261019T093000\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nSUMMARY:Sync"
            )
        );
        let o = feed(&body).occurrences(day(2026, 10, 1), day(2026, 12, 1), &Z0, 100);
        // Oct 19 BST (08:00 UTC), Oct 26 GMT (09:00 UTC, clocks went back on the 25th), Nov 2 GMT.
        assert_eq!(
            starts(&o),
            vec![
                utc(2026, 10, 19, 8, 0),
                utc(2026, 10, 26, 9, 0),
                utc(2026, 11, 2, 9, 0)
            ]
        );
    }

    #[test]
    fn an_old_endless_series_is_expanded_quickly_into_a_window() {
        let cal = feed(&vevent(
            "DTSTART:20150105T090000Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 5), day(2026, 10, 8), &Z0, 100);
        assert_eq!(o.len(), 3);
        let cal = feed(&vevent(
            "DTSTART:20100105T090000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=1MO\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 1), day(2026, 11, 1), &Z0, 100);
        assert_eq!(starts(&o), vec![utc(2026, 10, 5, 9, 0)]);
        let cal = feed(&vevent(
            "DTSTART:19990105T090000Z\r\nRRULE:FREQ=YEARLY\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 1, 1), day(2027, 1, 1), &Z0, 100);
        assert_eq!(starts(&o), vec![utc(2026, 1, 5, 9, 0)]);
    }

    #[test]
    fn only_events_overlapping_the_window_are_returned() {
        let cal = feed(&vevent(
            "DTSTART:20261006T230000Z\r\nDTEND:20261007T010000Z\r\nSUMMARY:overnight",
        ));
        // Starts before the window, ends inside it.
        assert_eq!(
            cal.occurrences(day(2026, 10, 7), day(2026, 10, 8), &Z0, 10)
                .len(),
            1
        );
        assert!(
            cal.occurrences(day(2026, 10, 7) + 3600, day(2026, 10, 8), &Z0, 10)
                .is_empty()
        );
        assert!(
            cal.occurrences(day(2026, 10, 1), day(2026, 10, 6), &Z0, 10)
                .is_empty()
        );
    }

    #[test]
    fn output_is_sorted_and_capped() {
        let a = vevent("DTSTART:20261009T100000Z\r\nSUMMARY:b");
        let b = "BEGIN:VEVENT\r\nUID:u2\r\nDTSTART:20261007T100000Z\r\nSUMMARY:a\r\nEND:VEVENT";
        let o =
            feed(&format!("{a}\r\n{b}")).occurrences(day(2026, 10, 1), day(2026, 11, 1), &Z0, 10);
        assert_eq!(
            o.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let cal = feed(&vevent(
            "DTSTART:20261001T090000Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:x",
        ));
        assert_eq!(
            cal.occurrences(day(2026, 10, 1), day(2027, 10, 1), &Z0, 5)
                .len(),
            5
        );
    }

    #[test]
    fn durations_and_missing_ends() {
        let cal = feed(&vevent(
            "DTSTART:20261006T130000Z\r\nDURATION:PT1H30M\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 10, 6), day(2026, 10, 7), &Z0, 10);
        assert_eq!(o[0].end_utc - o[0].start_utc, 5400);
        let cal = feed(&vevent("DTSTART:20261006T130000Z\r\nSUMMARY:instant"));
        let o = cal.occurrences(day(2026, 10, 6), day(2026, 10, 7), &Z0, 10);
        assert_eq!(o[0].end_utc, o[0].start_utc);
        assert_eq!(parse_duration("P1DT2H"), Some(DAY + 7200));
        assert_eq!(parse_duration("P2W"), Some(14 * DAY));
        assert_eq!(parse_duration("garbage"), None);
        assert_eq!(parse_duration("PT5X"), None);
    }

    #[test]
    fn garbage_in_does_not_panic_or_invent_events() {
        for text in [
            "",
            "not a calendar",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:2026\r\nEND:VEVENT\r\nEND:VCALENDAR",
            "BEGIN:VEVENT\r\nDTSTART:20261306T250000Z\r\nEND:VEVENT",
            "BEGIN:VEVENT\r\nDTSTART:20261006T130000Z\r\nRRULE:FREQ=\r\nEND:VEVENT",
            "BEGIN:VEVENT\r\nDTSTART:20261006T130000Z\r\nRRULE:FREQ=DAILY;COUNT=abc\r\nEND:VEVENT",
            "END:VEVENT\r\nEND:VEVENT\r\nBEGIN:VEVENT",
            "BEGIN:VEVENT\r\nDTSTART:20261006T130000Z",
            "\u{0}\u{1}\u{2}:::;;;\r\n\r\n   \r\n",
        ] {
            let cal = Calendar::parse(text);
            let _ = cal.occurrences(day(2026, 1, 1), day(2027, 1, 1), &Z0, 100);
        }
        assert!(Calendar::parse("garbage").is_empty());
    }

    #[test]
    fn pathological_rules_stay_bounded() {
        let cal = feed(&vevent(
            "DTSTART:20000101T090000Z\r\nRRULE:FREQ=YEARLY;INTERVAL=1000;COUNT=100000;BYMONTH=2;BYMONTHDAY=30\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2000, 1, 1), day(2100, 1, 1), &Z0, 100);
        assert!(o.len() <= 100);
        let cal = feed(&vevent(
            "DTSTART:20000101T090000Z\r\nRRULE:FREQ=DAILY;COUNT=100000\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2000, 1, 1), day(2030, 1, 1), &Z0, 100_000);
        assert!(o.len() <= MAX_PER_EVENT, "{}", o.len());
    }

    #[test]
    fn unsupported_rules_show_only_the_first_instance() {
        let cal = feed(&vevent(
            "DTSTART:20261001T090000Z\r\nRRULE:FREQ=YEARLY;BYWEEKNO=20\r\nSUMMARY:x",
        ));
        let o = cal.occurrences(day(2026, 1, 1), day(2032, 1, 1), &Z0, 100);
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn join_links_come_from_known_hosts_only() {
        let g = "Join: https://meet.google.com/abc-defg-hij.";
        assert_eq!(
            find_join_url(g).as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
        let z = "Zoom <https://us02web.zoom.us/j/123456789?pwd=abc>";
        assert_eq!(
            find_join_url(z).as_deref(),
            Some("https://us02web.zoom.us/j/123456789?pwd=abc")
        );
        let t = "Join the meeting now\nhttps://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0?context=%7b%7d\nMeeting ID: 1";
        assert!(
            find_join_url(t)
                .unwrap()
                .starts_with("https://teams.microsoft.com/l/meetup-join/")
        );
        // Not conferencing hosts, look-alikes, user-info tricks, plain http.
        for bad in [
            "https://evil.example/zoom.us",
            "https://notzoom.us/j/1",
            "https://zoom.us.evil.example/j/1",
            "https://meet.google.com@evil.example/x",
            "https://evil.example:443@zoom.us/j/1",
            "http://meet.google.com/abc",
            "https://meet.google.com:8443/x",
            "no links here",
        ] {
            assert_eq!(find_join_url(bad), None, "{bad}");
        }
        // The first allowed link wins even after a disallowed one.
        assert_eq!(
            find_join_url("see https://example.com/a then https://whereby.com/room").as_deref(),
            Some("https://whereby.com/room")
        );
    }

    #[test]
    fn join_urls_are_found_in_conference_properties_location_and_description() {
        let cal = feed(&vevent(
            "DTSTART:20261006T130000Z\r\nSUMMARY:x\r\nX-GOOGLE-CONFERENCE:https://meet.google.com/aaa-bbbb-ccc\r\nLOCATION:https://zoom.us/j/1",
        ));
        let o = cal.occurrences(day(2026, 10, 6), day(2026, 10, 7), &Z0, 10);
        assert_eq!(
            o[0].join_url.as_deref(),
            Some("https://meet.google.com/aaa-bbbb-ccc"),
            "the conference property wins"
        );
        let cal = feed(&vevent(
            "DTSTART:20261006T130000Z\r\nSUMMARY:x\r\nDESCRIPTION:Agenda\\nJoin https://zoom.us/j/42\\nThanks",
        ));
        let o = cal.occurrences(day(2026, 10, 6), day(2026, 10, 7), &Z0, 10);
        assert_eq!(o[0].join_url.as_deref(), Some("https://zoom.us/j/42"));
        let cal = feed(&vevent(
            "DTSTART:20261006T130000Z\r\nSUMMARY:x\r\nLOCATION:Room 4",
        ));
        assert_eq!(
            cal.occurrences(day(2026, 10, 6), day(2026, 10, 7), &Z0, 10)[0].join_url,
            None
        );
    }

    #[test]
    fn offsets_parse() {
        assert_eq!(parse_offset("+0100"), Some(3600));
        assert_eq!(parse_offset("-0530"), Some(-(5 * 3600 + 1800)));
        assert_eq!(parse_offset("+013000"), Some(3600 + 1800));
        assert_eq!(parse_offset("0100"), None);
    }

    #[test]
    fn nth_weekday_helpers() {
        // October 2026: Thursdays are 1, 8, 15, 22, 29.
        assert_eq!(
            nth_weekday(2026, 10, 1, 3),
            Some(days_from_civil(2026, 10, 1))
        );
        assert_eq!(
            nth_weekday(2026, 10, -1, 3),
            Some(days_from_civil(2026, 10, 29))
        );
        assert_eq!(
            nth_weekday(2026, 10, 5, 3),
            Some(days_from_civil(2026, 10, 29))
        );
        assert_eq!(
            nth_weekday(2026, 10, 5, 0),
            None,
            "no fifth Monday in Oct 2026"
        );
        assert_eq!(month_day(2026, 2, 30), None);
        assert_eq!(month_day(2026, 2, -1), Some(days_from_civil(2026, 2, 28)));
    }
}

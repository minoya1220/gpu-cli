//! Local deadline parsing and SSH transport; no account credentials cross SSH.

use std::sync::LazyLock;

use jiff::civil::{Date, DateTime};
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};
use jiff::{Span, Timestamp};
use regex::Regex;

use crate::ctx::{Ctx, Env};
use crate::error::{Error, Result};
use crate::process::{Cmd, Input, RunError};
use crate::pyfmt::{is_digits, py_float, shell_join, shell_quote};
use crate::pyjson::{self, Dict, Value};

/// The remote controller, sent over SSH stdin and run with the instance's bash.
pub const REMOTE_SOURCE: &str = include_str!("../remote/expiry_remote.sh");

/// The boot-time guard the onstart script starts (`crate::startup::expiry_guard`).
pub const GUARD_SOURCE: &str = include_str!("../remote/expiry_guard.sh");

static DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:([0-9]+)d)?(?:([0-9]+)h)?(?:([0-9]+)m)?(?:([0-9]+)s)?$").unwrap()
});
static DURATION_SHAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[0-9]+[dhms])+$").unwrap());
static CLOCK_12: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]{1,2})(?::([0-9]{2}))?([aApP][mM])$").unwrap());
static CLOCK_24: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{1,2}:[0-9]{2}$").unwrap());

const OUT_OF_RANGE: &str = "Expiry is out of range.";

/// Seconds in a duration such as `1h30m`. Values too large for any date
/// still parse; turning them into a deadline reports them as out of range.
pub fn parse_duration(value: &str) -> std::result::Result<u128, String> {
    let invalid = || "Use a duration such as 30m, 2h, or 1h30m (units: d h m s).".to_string();
    let captures = DURATION.captures(value).ok_or_else(invalid)?;
    if (1..=4).all(|i| captures.get(i).is_none()) {
        return Err(invalid());
    }
    let mut seconds: u128 = 0;
    for (i, unit) in [(1, 86400u128), (2, 3600), (3, 60), (4, 1)] {
        if let Some(n) = captures.get(i) {
            let n = n.as_str().bytes().fold(0u128, |acc, b| {
                acc.saturating_mul(10).saturating_add(u128::from(b - b'0'))
            });
            seconds = seconds.saturating_add(n.saturating_mul(unit));
        }
    }
    if seconds == 0 {
        return Err("Expiry duration must be positive.".into());
    }
    Ok(seconds)
}

/// Split `[ID] WHEN|cancel|show`; a bare number is always an ID.
pub fn parse_expire_arguments(
    values: &[String],
) -> std::result::Result<(Option<String>, String), String> {
    let mut values = values.to_vec();
    let id = if values.first().is_some_and(|v| {
        is_digits(v)
            || v.starts_with("gcp:")
            || (values.len() == 2 && crate::provider::short_candidate(v))
    }) {
        Some(values.remove(0))
    } else {
        None
    };
    if values.len() != 1 || is_digits(&values[0]) {
        return Err(
            "Use gpu expire [ID] WHEN|cancel|show; e.g. 2h, 18:30, 6:30PM. Bare numbers are IDs, not times.".into(),
        );
    }
    Ok((id, values.remove(0)))
}

pub fn local_zone(env: &Env) -> TimeZone {
    let Some(tz) = env.get("TZ") else {
        return TimeZone::try_system().unwrap_or(TimeZone::UTC);
    };
    let name = tz.strip_prefix(':').unwrap_or(tz);
    if name.is_empty() {
        return TimeZone::UTC;
    }
    if name.starts_with('/') {
        if let Ok(data) = std::fs::read(name) {
            if let Ok(zone) = TimeZone::tzif(name, &data) {
                return zone;
            }
        }
    }
    if let Ok(zone) = jiff::tz::db().get(name) {
        return zone;
    }
    TimeZone::posix(tz).unwrap_or(TimeZone::UTC)
}

/// Instants a local wall time can mean: none in a DST gap, two in a fold.
fn local_candidates(
    wall: DateTime,
    zone: &TimeZone,
) -> std::result::Result<Vec<Timestamp>, String> {
    let at = |offset: Offset| {
        offset
            .to_timestamp(wall)
            .map_err(|_| OUT_OF_RANGE.to_string())
    };
    Ok(match zone.to_ambiguous_timestamp(wall).offset() {
        AmbiguousOffset::Unambiguous { offset } => vec![at(offset)?],
        AmbiguousOffset::Gap { .. } => Vec::new(),
        AmbiguousOffset::Fold { before, after } => {
            let mut both = vec![at(before)?, at(after)?];
            both.sort();
            both.dedup();
            both
        }
    })
}

/// A deadline from a duration (`2h`), clock time (`18:30`, `6pm`) or ISO
/// datetime. `zone` defaults to the local zone.
pub fn parse_when(
    value: &str,
    now: Timestamp,
    zone: &TimeZone,
) -> std::result::Result<Timestamp, String> {
    if DURATION_SHAPE.is_match(value) {
        parse_deadline_duration(value, now)
    } else {
        parse_deadline_at(value, now, zone)
    }
}

pub fn parse_deadline_duration(
    value: &str,
    now: Timestamp,
) -> std::result::Result<Timestamp, String> {
    let seconds = parse_duration(value)?;
    let seconds = i64::try_from(seconds).map_err(|_| OUT_OF_RANGE.to_string())?;
    let span = Span::new()
        .try_seconds(seconds)
        .map_err(|_| OUT_OF_RANGE.to_string())?;
    now.checked_add(span).map_err(|_| OUT_OF_RANGE.to_string())
}

fn parse_deadline_at(
    at: &str,
    now: Timestamp,
    zone: &TimeZone,
) -> std::result::Result<Timestamp, String> {
    let clock = CLOCK_12.captures(at);
    if clock.is_some() || CLOCK_24.is_match(at) {
        let (hour, minute) = match &clock {
            Some(caps) => {
                let hour: i64 = caps[1].parse().unwrap_or(0);
                let minute: i64 = caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0));
                if !(1..=12).contains(&hour) {
                    return Err("12-hour clock hours must be 1\u{2013}12.".into());
                }
                (
                    hour % 12
                        + if caps[3].eq_ignore_ascii_case("pm") {
                            12
                        } else {
                            0
                        },
                    minute,
                )
            }
            None => {
                let (h, m) = at.split_once(':').unwrap_or((at, "0"));
                (h.parse().unwrap_or(0), m.parse().unwrap_or(0))
            }
        };
        if !(0..=23).contains(&hour) {
            return Err("hour must be in 0..23".into());
        }
        if !(0..=59).contains(&minute) {
            return Err("minute must be in 0..59".into());
        }
        let day = now.to_zoned(zone.clone()).date();
        // Look a few days ahead to skip a nonexistent spring-forward wall time.
        for offset in 0..3 {
            let date = day
                .checked_add(Span::new().days(offset))
                .map_err(|_| OUT_OF_RANGE.to_string())?;
            let wall = date.at(hour as i8, minute as i8, 0, 0);
            if let Some(found) = local_candidates(wall, zone)?.into_iter().find(|t| *t > now) {
                return Ok(found);
            }
        }
        return Err("No next occurrence found; use an ISO datetime with UTC offset.".into());
    }
    if !at.contains('T') && !at.contains(' ') {
        return Err("Use HH:MM or an ISO datetime, preferably with UTC offset.".into());
    }
    let text = at.replace('Z', "+00:00");
    let parsed = fromisoformat(&text)?;
    let value = match parsed.offset {
        Some(offset) => offset
            .to_timestamp(parsed.wall)
            .map_err(|_| OUT_OF_RANGE.to_string())?,
        None => {
            let candidates = local_candidates(parsed.wall, zone)?;
            if candidates.len() != 1 {
                return Err(
                    "Local time is nonexistent or ambiguous (DST); include a UTC offset.".into(),
                );
            }
            candidates[0]
        }
    };
    if value <= now {
        return Err("Expiry time must be in the future.".into());
    }
    Ok(value)
}

/// A parsed ISO datetime: wall time plus an optional UTC offset.
pub struct IsoDateTime {
    pub wall: DateTime,
    pub offset: Option<Offset>,
}

fn byte_at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// Exactly `n` ASCII digits at `pos`.
fn digits(s: &[u8], pos: usize, n: usize) -> Option<(i64, usize)> {
    let mut value = 0i64;
    for i in 0..n {
        let b = byte_at(s, pos + i);
        if !b.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(b - b'0');
    }
    Some((value, pos + n))
}

fn separator_position(s: &[u8]) -> Option<usize> {
    let len = s.len();
    if len == 7 {
        return Some(7);
    }
    if byte_at(s, 4) == b'-' {
        if byte_at(s, 5) == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && byte_at(s, 8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && byte_at(s, 10).is_ascii_digit() {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if byte_at(s, 4) == b'W' {
        let mut idx = 7;
        while idx < len && s[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        return Some(if idx % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

enum DateError {
    Invalid,
    Range(String),
}

fn iso_week_date(
    year: i64,
    week: i64,
    weekday: i64,
) -> std::result::Result<(i64, i64, i64), DateError> {
    if !(1..=9999).contains(&year) {
        return Err(DateError::Range(format!("year {year} is out of range")));
    }
    let jan4 = Date::new(year as i16, 1, 4).map_err(|_| DateError::Invalid)?;
    let has_53 = {
        let dec28 = Date::new(year as i16, 12, 28).map_err(|_| DateError::Invalid)?;
        dec28.iso_week_date().week() == 53
    };
    if !(1..=52).contains(&week) && !(week == 53 && has_53) {
        return Err(DateError::Invalid);
    }
    if !(1..=7).contains(&weekday) {
        return Err(DateError::Invalid);
    }
    let monday = jan4
        .checked_sub(Span::new().days(i64::from(jan4.weekday().to_monday_one_offset()) - 1))
        .map_err(|_| DateError::Invalid)?;
    let date = monday
        .checked_add(Span::new().days((week - 1) * 7 + weekday - 1))
        .map_err(|_| DateError::Range(format!("Year is out of range: {year}")))?;
    Ok((
        i64::from(date.year()),
        i64::from(date.month()),
        i64::from(date.day()),
    ))
}

fn parse_date(s: &[u8], end: usize) -> std::result::Result<(i64, i64, i64), DateError> {
    let (year, mut p) = digits(s, 0, 4).ok_or(DateError::Invalid)?;
    let separator = byte_at(s, p) == b'-';
    if separator {
        p += 1;
    }
    if byte_at(s, p) == b'W' {
        p += 1;
        let (week, after) = digits(s, p, 2).ok_or(DateError::Invalid)?;
        p = after;
        let weekday = if p < end {
            if separator {
                if byte_at(s, p) != b'-' {
                    return Err(DateError::Invalid);
                }
                p += 1;
            }
            digits(s, p, 1).ok_or(DateError::Invalid)?.0
        } else {
            1
        };
        return iso_week_date(year, week, weekday);
    }
    let (month, after) = digits(s, p, 2).ok_or(DateError::Invalid)?;
    p = after;
    if separator {
        if byte_at(s, p) != b'-' {
            return Err(DateError::Invalid);
        }
        p += 1;
    }
    let (day, _) = digits(s, p, 2).ok_or(DateError::Invalid)?;
    Ok((year, month, day))
}

fn parse_hms(s: &[u8], start: usize, end: usize) -> Option<([i64; 4], bool)> {
    let mut values = [0i64; 4];
    let mut p = start;
    let mut has_separator = true;
    for i in 0..3 {
        let (v, after) = digits(s, p, 2)?;
        values[i] = v;
        p = after;
        let c = byte_at(s, p);
        p += 1;
        if i == 0 {
            has_separator = c == b':';
        }
        if p >= end {
            return Some((values, c != 0));
        } else if has_separator && c == b':' {
            continue;
        } else if c == b'.' || c == b',' {
            break;
        } else if !has_separator {
            p -= 1;
        } else {
            return None;
        }
    }
    let take = end.saturating_sub(p).min(6);
    let (micro, after) = digits(s, p, take)?;
    p = after;
    values[3] = micro * 10i64.pow((6 - take) as u32);
    while byte_at(s, p).is_ascii_digit() {
        p += 1;
    }
    Some((values, byte_at(s, p) != 0))
}

fn invalid_iso(text: &str) -> String {
    format!(
        "Invalid datetime {}. Use YYYY-MM-DDTHH:MM:SS+HH:MM.",
        crate::pyfmt::repr_str(text)
    )
}

pub fn fromisoformat(text: &str) -> std::result::Result<IsoDateTime, String> {
    let s = text.as_bytes();
    let invalid = || invalid_iso(text);
    let separator = separator_position(s).ok_or_else(invalid)?;
    let (year, month, day) = match parse_date(s, separator) {
        Ok(ymd) => ymd,
        Err(DateError::Invalid) => return Err(invalid()),
        Err(DateError::Range(message)) => return Err(message),
    };
    let mut time = [0i64; 4];
    let mut offset = None;
    if s.len() > separator {
        let start = separator + 1;
        let end = s.len();
        let tz = (start..end)
            .find(|&i| matches!(s[i], b'Z' | b'+' | b'-'))
            .unwrap_or(end);
        let (values, trailing) = parse_hms(&s[..tz], start, tz).ok_or_else(invalid)?;
        time = values;
        if tz == end {
            if trailing {
                return Err(invalid());
            }
        } else if s[tz] == b'Z' {
            if tz + 1 != end {
                return Err(invalid());
            }
            offset = Some(0i64);
        } else {
            let sign = if s[tz] == b'-' { -1 } else { 1 };
            let (tz_values, tz_trailing) = parse_hms(s, tz + 1, end).ok_or_else(invalid)?;
            if tz_trailing {
                return Err(invalid());
            }
            let micros =
                (tz_values[0] * 3600 + tz_values[1] * 60 + tz_values[2]) * 1_000_000 + tz_values[3];
            if micros >= 86_400_000_000 {
                return Err(
                    "UTC offset must be between -24 and +24 hours, excluding both limits.".into(),
                );
            }
            if micros % 1_000_000 != 0 {
                return Err(invalid());
            }
            offset = Some(sign * micros / 1_000_000);
        }
    }
    if !(1..=9999).contains(&year) {
        return Err(format!("year {year} is out of range"));
    }
    if !(1..=12).contains(&month) {
        return Err("month must be in 1..12".into());
    }
    let date = Date::new(year as i16, month as i8, 1).map_err(|_| invalid())?;
    if day < 1 || day > i64::from(date.days_in_month()) {
        return Err("day is out of range for month".into());
    }
    let checks = [("hour", 23), ("minute", 59), ("second", 59)];
    for (value, (name, max)) in time.iter().zip(checks) {
        if !(0..=max).contains(value) {
            return Err(format!("{name} must be in 0..{max}"));
        }
    }
    let wall = DateTime::new(
        year as i16,
        month as i8,
        day as i8,
        time[0] as i8,
        time[1] as i8,
        time[2] as i8,
        (time[3] * 1000) as i32,
    )
    .map_err(|_| invalid())?;
    let offset = match offset {
        Some(seconds) => Some(Offset::from_seconds(seconds as i32).map_err(|_| invalid())?),
        None => None,
    };
    Ok(IsoDateTime { wall, offset })
}

/// The `ssh` argv; the remote command line holds only validated words.
pub fn remote_command(
    alias: &str,
    instance_id: &str,
    action: &str,
    deadline: Option<Timestamp>,
) -> Result<Vec<String>> {
    if !is_digits(instance_id) || alias != format!("vast-{instance_id}") {
        return Err(Error::msg("Invalid managed instance SSH alias or ID."));
    }
    if !matches!(action, "arm" | "show" | "cancel") {
        return Err(Error::msg("Invalid expiry action."));
    }
    let mut args = vec![
        "bash".to_string(),
        "-s".into(),
        "--".into(),
        action.into(),
        instance_id.into(),
    ];
    if action == "arm" {
        let deadline = deadline.ok_or_else(|| Error::msg("Arming requires a deadline."))?;
        args.push(deadline_seconds(deadline).to_string());
    }
    Ok(crate::ssh::command(&["ssh"], false, alias, &shell_join(&args)).argv)
}

/// A deadline as whole epoch seconds, rounded up so it is never early.
pub fn deadline_seconds(deadline: Timestamp) -> i64 {
    deadline.as_second() + i64::from(deadline.subsec_nanosecond() > 0)
}

/// The script sent on stdin: the source as a variable, then the source.
pub fn transport_payload() -> String {
    format!(
        "GPU_EXPIRE_SOURCE={}\n{REMOTE_SOURCE}",
        shell_quote(REMOTE_SOURCE)
    )
}

pub fn manage_expiry(
    ctx: &Ctx,
    alias: &str,
    instance_id: &str,
    action: &str,
    deadline: Option<Timestamp>,
) -> Result<Dict> {
    let command = remote_command(alias, instance_id, action, deadline)?;
    let cmd = Cmd::new(&command)
        .stdin(Input::Text(transport_payload()))
        .timeout(Some(45.0));
    let result =
        match ctx.run(&cmd) {
            Ok(result) => result,
            Err(RunError::TimedOut) => return Err(Error::msg(
                format!("Expiry SSH timed out; arming/cancellation is unverified. Run gpu expire {instance_id} show."),
            )),
            Err(other) => return Err(Error::msg(other.python_text("ssh"))),
        };
    // Never echo SSH stderr or arbitrary remote text: it may contain secrets.
    let reply = pyjson::loads(&result.stdout).unwrap_or(Value::Dict(Dict::new()));
    let message = |code: &str| {
        match code {
        "credentials" => "Instance-scoped CONTAINER_ID/CONTAINER_API_KEY missing or mismatched; expiry NOT armed.".to_string(),
        "past" => format!("Deadline passed before SSH was ready; expiry NOT armed. Run gpu expire {instance_id} 2h."),
        "verification" => format!("Timer startup verification failed; new expiry NOT armed (previous deadline unchanged). Run gpu expire {instance_id} show."),
        _ => format!("Remote expiry operation failed; check bash, curl, flock and writable ~/.local/state/gpu-expire; run gpu expire {instance_id} show."),
    }
    };
    let error = reply.get("error").filter(|e| crate::pyfmt::truthy(e));
    let Value::Dict(reply) = reply.clone() else {
        return Err(Error::msg(message("remote")));
    };
    if result.code != 0 || error.is_some() {
        let code = error.and_then(Value::as_str).unwrap_or("remote");
        return Err(Error::msg(message(code)));
    }
    let status = reply.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        status,
        "armed" | "retrying" | "inactive" | "none" | "cancelled" | "destroy-request-accepted"
    ) {
        return Err(Error::msg(format!(
            "Invalid expiry response; completion unverified. Run gpu expire {instance_id} show."
        )));
    }
    if action == "arm" && status != "armed" {
        return Err(Error::msg(message("verification")));
    }
    Ok(reply)
}

fn iso_offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let total = seconds.unsigned_abs();
    let (hours, minutes, secs) = (total / 3600, total % 3600 / 60, total % 60);
    if secs == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{secs:02}")
    }
}

/// A deadline in the local zone, as `2026-01-02T18:30:00-05:00 EST`.
pub fn local_time(ctx: &Ctx, at: Timestamp) -> String {
    let local = at.to_zoned(local_zone(&ctx.env));
    format!(
        "{}{} {}",
        local.datetime().strftime("%Y-%m-%dT%H:%M:%S"),
        iso_offset(local.offset().seconds()),
        local.strftime("%Z")
    )
}

pub fn expiry_summary(ctx: &Ctx, instance_id: &str, reply: &Dict) -> Result<String> {
    let status = crate::pyfmt::py_str(reply.get("status").ok_or_else(|| Error::key("status"))?);
    if status == "none" || status == "cancelled" {
        let suffix = if status == "cancelled" {
            " (cancelled)."
        } else {
            "."
        };
        return Ok(format!(
            "Instance {instance_id}: no auto-destroy deadline{suffix}"
        ));
    }
    let deadline = reply
        .get("deadline")
        .ok_or_else(|| Error::key("deadline"))?;
    let seconds = py_float(deadline).map_err(|_| {
        Error::msg(format!(
            "Expiry deadline must be a number, got {}. Run gpu expire {instance_id} show.",
            deadline.kind()
        ))
    })?;
    let micros = (seconds * 1e6).round_ties_even();
    let deadline = Timestamp::from_microsecond(micros as i64)
        .map_err(|_| Error::msg("year is out of range"))?;
    let text = local_time(ctx, deadline);
    let remaining = deadline.as_microsecond() - ctx.clock.now().as_microsecond();
    let total = if remaining <= 0 {
        0
    } else {
        (remaining + 999_999) / 1_000_000
    };
    let (hours, rest) = (total / 3600, total % 3600);
    let (minutes, secs) = (rest / 60, rest % 60);
    Ok(format!(
        "Instance {instance_id}: auto-DESTROY at {text} (in {hours}h {minutes}m {secs}s); timer {status}. Disk \
            will be deleted; all billing stops on destruction."
    ))
}

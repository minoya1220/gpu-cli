//! Python's text formatting for the values the tool prints or passes as
//! arguments: `str()`, `repr()`, float repr, `%.3f`, `%`, `g`, `float()`,
//! truthiness, `str.isprintable` and `shlex.quote`.

use std::fmt::Write as _;

use crate::pyjson::Value;

/// `repr(float)`: shortest round-trip digits, fixed notation for decimal
/// exponents in -4..16, scientific otherwise, always with a `.` or `e`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    let (digits, exponent) = shortest_digits(f.abs());
    let sign = if f < 0.0 { "-" } else { "" };
    let point = exponent + 1;
    if -4 < point && point <= 16 {
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            let (whole, frac) = digits.split_at(point as usize);
            format!("{whole}.{frac}")
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_string()
        } else {
            format!("{first}.{rest}")
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{exp_sign}{:02}", exponent.abs())
    }
}

/// Significant digits and decimal exponent of `x` in scientific notation.
fn sci_digits(sci: &str) -> (String, i32) {
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci, "0"));
    let digits = mantissa.chars().filter(char::is_ascii_digit).collect();
    (digits, exponent.parse().unwrap_or(0))
}

/// Shortest round-trip digits of a positive finite `x`. Rust breaks an exact
/// tie between two shortest candidates differently from CPython, which rounds
/// half to even, so a tie is detected from the exact expansion and resolved.
fn shortest_digits(x: f64) -> (String, i32) {
    let (digits, exponent) = sci_digits(&format!("{x:e}"));
    let (exact, exact_exponent) = sci_digits(&format!("{x:.800e}"));
    let k = digits.len();
    let tie = exact_exponent == exponent
        && exact.as_bytes()[k] == b'5'
        && exact[k + 1..].bytes().all(|b| b == b'0');
    if !tie {
        return (digits, exponent);
    }
    let lower = &exact[..k];
    let even = if matches!(lower.as_bytes()[k - 1], b'0' | b'2' | b'4' | b'6' | b'8') {
        lower.to_string()
    } else {
        match increment(lower) {
            Some(upper) => upper,
            None => return (digits, exponent),
        }
    };
    let (first, rest) = even.split_at(1);
    let round_trips = format!("{first}.{rest}0e{exponent}").parse::<f64>() == Ok(x);
    if !round_trips {
        return (digits, exponent);
    }
    let trimmed = even.trim_end_matches('0');
    let even = if trimmed.is_empty() { "0" } else { trimmed };
    (even.to_string(), exponent)
}

/// A decimal digit string plus one in its last place; None on carry out.
fn increment(digits: &str) -> Option<String> {
    let mut bytes = digits.as_bytes().to_vec();
    for b in bytes.iter_mut().rev() {
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

fn special(f: f64) -> Option<String> {
    if f.is_nan() {
        Some("nan".into())
    } else if f.is_infinite() {
        Some(if f > 0.0 { "inf".into() } else { "-inf".into() })
    } else {
        None
    }
}

/// `f"{x:.{precision}f}"`.
pub fn fixed(f: f64, precision: usize) -> String {
    special(f).unwrap_or_else(|| format!("{f:.precision$}"))
}

/// `f"{x:.{precision}%}"`: multiplied by 100 in floating point, then fixed.
pub fn percent(f: f64, precision: usize) -> String {
    format!("{}%", fixed(f * 100.0, precision))
}

/// `f"{x:g}"`: six significant digits, trailing zeros removed.
pub fn general(f: f64) -> String {
    if let Some(s) = special(f) {
        return s;
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let sci = format!("{f:.5e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let strip = |s: String| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if (-4..6).contains(&exponent) {
        strip(format!("{:.*}", (5 - exponent) as usize, f))
    } else {
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!(
            "{}e{exp_sign}{:02}",
            strip(mantissa.to_string()),
            exponent.abs()
        )
    }
}

/// `str(value)` for a JSON value.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::Str(s) => s.clone(),
        other => repr(other),
    }
}

/// `repr(value)` for a JSON value.
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(digits) => digits.clone(),
        Value::Float(f) => float_repr(*f),
        Value::Str(s) => repr_str(s),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Dict(dict) => {
            let parts: Vec<String> = dict
                .iter()
                .map(|(k, v)| format!("{}: {}", repr_str(k), repr(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// `repr(str)`.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if c.is_ascii() || is_printable(c) => out.push(c),
            c if (c as u32) < 0x100 => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if (c as u32) < 0x10000 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let _ = write!(out, "\\U{:08x}", c as u32);
            }
        }
    }
    out.push(quote);
    out
}

/// Python's name for a value's type, as used in its error messages.
pub fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) | Value::BigInt(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
    }
}

/// Python truthiness.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::BigInt(_) => true,
        Value::Float(f) => *f != 0.0,
        Value::Str(s) => !s.is_empty(),
        Value::List(l) => !l.is_empty(),
        Value::Dict(d) => !d.is_empty(),
    }
}

/// `float(value)`, with Python's error messages.
pub fn py_float(value: &Value) -> Result<f64, String> {
    match value {
        Value::Bool(b) => Ok(f64::from(u8::from(*b))),
        Value::Int(i) => Ok(*i as f64),
        Value::BigInt(digits) => Ok(digits.parse().unwrap_or(f64::NAN)),
        Value::Float(f) => Ok(*f),
        Value::Str(s) => parse_float(s)
            .ok_or_else(|| format!("could not convert string to float: {}", repr_str(s))),
        other => Err(format!(
            "float() argument must be a string or a real number, not '{}'",
            type_name(other)
        )),
    }
}

/// `float(str)`: surrounding whitespace, underscores between digits, and
/// `inf`/`infinity`/`nan` in any case are accepted.
pub fn parse_float(s: &str) -> Option<f64> {
    let s = s.trim();
    let chars: Vec<char> = s.chars().collect();
    let mut cleaned = String::with_capacity(s.len());
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' {
            let digit_before = i > 0 && chars[i - 1].is_ascii_digit();
            let digit_after = chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if !(digit_before && digit_after) {
                return None;
            }
        } else {
            cleaned.push(*c);
        }
    }
    let body = cleaned.trim_start_matches(['+', '-']);
    let lower = body.to_ascii_lowercase();
    let word = matches!(lower.as_str(), "inf" | "infinity" | "nan");
    let numeric = !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && body
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.');
    if cleaned.len() - body.len() > 1 || !(word || numeric) {
        return None;
    }
    cleaned.parse::<f64>().ok()
}

/// `str.isprintable()` for one character. Approximation: control, format
/// and separator characters (other than space) are not printable;
/// unassigned and private-use code points are.
pub fn is_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    let code = c as u32;
    let format = matches!(code,
        0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x890..=0x891 | 0x8e2 | 0x180e
        | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x206f
        | 0xfeff | 0xfff9..=0xfffb | 0x110bd | 0x110cd | 0x13430..=0x1343f
        | 0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f);
    !format
}

pub fn is_printable_str(s: &str) -> bool {
    s.chars().all(is_printable)
}

/// `shlex.quote`.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let safe = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// `shlex.quote(value)` for a JSON value: falsy values quote as `''`, other
/// non-strings fail with Python's `TypeError` text.
pub fn shell_quote_value(value: &Value) -> Result<String, String> {
    match value {
        Value::Str(s) => Ok(shell_quote(s)),
        other if !truthy(other) => Ok("''".into()),
        other => Err(format!(
            "expected string or bytes-like object, got {}",
            repr_str(type_name(other))
        )),
    }
}

/// `shlex.join`.
pub fn shell_join<S: AsRef<str>>(args: &[S]) -> String {
    args.iter()
        .map(|a| shell_quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `str.casefold()`, approximated by Unicode lowercase.
pub fn casefold(s: &str) -> String {
    s.to_lowercase()
}

/// `str.splitlines()`: splits on `\r\n` and every Unicode line boundary,
/// with no empty item after a final break.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            lines.push(&s[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|&(_, n)| n == '\n') {
                chars.next();
                start += 1;
            }
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// Whether a string is a non-empty run of ASCII digits (`str.isdigit()`,
/// restricted to ASCII).
pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

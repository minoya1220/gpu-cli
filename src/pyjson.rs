//! JSON that reads and writes like Python's `json` module.
//!
//! The Python tool keeps every JSON document as plain dicts and lists and
//! formats values with `str()`, so this module has its own value type rather
//! than typed structs. It accepts what `json.loads` accepts (including bare
//! `NaN` and `Infinity`) and `dumps` reproduces `json.dumps` byte for byte for
//! the indent styles the tool uses.

use std::fmt::Write as _;

use crate::pyfmt::float_repr;

/// A JSON value with Python semantics: object keys keep insertion order and a
/// repeated key keeps its first position with the last value, like a dict.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// An integer outside `i64`, kept as its exact decimal digits.
    BigInt(String),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Dict(Dict),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict(Vec<(String, Value)>);

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Set a key; an existing key keeps its position, a new one goes last.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, Value)> for Dict {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut dict = Dict::new();
        for (key, value) in iter {
            dict.insert(key, value);
        }
        dict
    }
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    /// A plain-language noun phrase for the value's kind, for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "a boolean",
            Value::Int(_) | Value::BigInt(_) | Value::Float(_) => "a number",
            Value::Str(_) => "text",
            Value::List(_) => "a list",
            Value::Dict(_) => "an object",
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<Value>> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    /// `dict.get(key)` for dicts; None for anything else.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict().and_then(|d| d.get(key))
    }

    /// Python `==` between JSON values: `1 == 1.0 == True`.
    pub fn py_eq(&self, other: &Value) -> bool {
        match (self.as_number(), other.as_number()) {
            (Some(a), Some(b)) => a == b,
            (None, None) => match (self, other) {
                (Value::List(a), Value::List(b)) => {
                    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.py_eq(y))
                }
                (Value::Dict(a), Value::Dict(b)) => {
                    a.len() == b.len()
                        && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| v.py_eq(w)))
                }
                _ => self == other,
            },
            _ => false,
        }
    }

    fn as_number(&self) -> Option<f64> {
        match self {
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            Value::Int(i) => Some(*i as f64),
            Value::BigInt(digits) => digits.parse().ok(),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

/// `json.loads`, with Python's error messages.
pub fn loads(text: &str) -> Result<Value, String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.first() == Some(&'\u{feff}') {
        return Err(error(
            &chars,
            "Unexpected UTF-8 BOM (decode using utf-8-sig)",
            0,
        ));
    }
    let mut parser = Parser { s: &chars, pos: 0 };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != chars.len() {
        return Err(error(&chars, "Extra data", parser.pos));
    }
    Ok(value)
}

fn error(s: &[char], msg: &str, pos: usize) -> String {
    let before = &s[..pos.min(s.len())];
    let line = before.iter().filter(|c| **c == '\n').count() + 1;
    let column = match before.iter().rposition(|c| *c == '\n') {
        Some(newline) => pos - newline,
        None => pos + 1,
    };
    format!("{msg}: line {line} column {column} (char {pos})")
}

struct Parser<'a> {
    s: &'a [char],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.pos).copied()
    }

    fn err(&self, msg: &str, pos: usize) -> String {
        error(self.s, msg, pos)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn starts_with(&self, word: &str) -> bool {
        word.chars()
            .enumerate()
            .all(|(i, c)| self.s.get(self.pos + i) == Some(&c))
    }

    fn value(&mut self) -> Result<Value, String> {
        let start = self.pos;
        match self.peek() {
            Some('"') => {
                self.pos += 1;
                Ok(Value::Str(self.string(start)?))
            }
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('n') if self.starts_with("null") => {
                self.pos += 4;
                Ok(Value::Null)
            }
            Some('t') if self.starts_with("true") => {
                self.pos += 4;
                Ok(Value::Bool(true))
            }
            Some('f') if self.starts_with("false") => {
                self.pos += 5;
                Ok(Value::Bool(false))
            }
            Some('N') if self.starts_with("NaN") => {
                self.pos += 3;
                Ok(Value::Float(f64::NAN))
            }
            Some('I') if self.starts_with("Infinity") => {
                self.pos += 8;
                Ok(Value::Float(f64::INFINITY))
            }
            Some('-') if self.starts_with("-Infinity") => {
                self.pos += 9;
                Ok(Value::Float(f64::NEG_INFINITY))
            }
            Some('-' | '0'..='9') => self.number(),
            _ => Err(self.err("Expecting value", start)),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some('0'..='9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        match self.peek() {
            Some('0') => self.pos += 1,
            Some('1'..='9') => {
                self.digits();
            }
            _ => return Err(self.err("Expecting value", start)),
        }
        let mut is_float = false;
        if self.peek() == Some('.') && matches!(self.s.get(self.pos + 1), Some('0'..='9')) {
            self.pos += 1;
            self.digits();
            is_float = true;
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let mark = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                self.pos = mark;
            } else {
                is_float = true;
            }
        }
        let text: String = self.s[start..self.pos].iter().collect();
        if !is_float {
            return Ok(match text.parse::<i64>() {
                Ok(i) => Value::Int(i),
                Err(_) => Value::BigInt(text),
            });
        }
        Ok(Value::Float(text.parse::<f64>().unwrap_or(f64::NAN)))
    }

    /// Parse after the opening quote; `start` is the quote's position.
    fn string(&mut self, start: usize) -> Result<String, String> {
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("Unterminated string starting at", start));
            };
            match c {
                '"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                '\\' => {
                    let Some(esc) = self.s.get(self.pos + 1).copied() else {
                        return Err(self.err("Unterminated string starting at", start));
                    };
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let first = self.hex4(self.pos + 2)?;
                            self.pos += 6;
                            let mut code = first;
                            if (0xd800..0xdc00).contains(&first)
                                && self.peek() == Some('\\')
                                && self.s.get(self.pos + 1) == Some(&'u')
                            {
                                if let Ok(second) = self.hex4(self.pos + 2) {
                                    if (0xdc00..0xe000).contains(&second) {
                                        code =
                                            0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                                        self.pos += 6;
                                    }
                                }
                            }
                            // Lone surrogates cannot live in a Rust string.
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            continue;
                        }
                        _ => return Err(self.err("Invalid \\escape", self.pos)),
                    }
                    self.pos += 2;
                }
                c if (c as u32) < 0x20 => {
                    return Err(self.err("Invalid control character at", self.pos));
                }
                c => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn hex4(&self, at: usize) -> Result<u32, String> {
        let digits: String = self.s.get(at..at + 4).unwrap_or(&[]).iter().collect();
        if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(u32::from_str_radix(&digits, 16).unwrap_or(0))
        } else {
            Err(self.err("Invalid \\uXXXX escape", at - 1))
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut dict = Dict::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Value::Dict(dict));
        }
        loop {
            if self.peek() != Some('"') {
                return Err(self.err(
                    "Expecting property name enclosed in double quotes",
                    self.pos,
                ));
            }
            let start = self.pos;
            self.pos += 1;
            let key = self.string(start)?;
            self.skip_ws();
            if self.peek() != Some(':') {
                return Err(self.err("Expecting ':' delimiter", self.pos));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            dict.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    return Ok(Value::Dict(dict));
                }
                Some(',') => {
                    let comma = self.pos;
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some('}') {
                        return Err(self.err("Illegal trailing comma before end of object", comma));
                    }
                }
                _ => return Err(self.err("Expecting ',' delimiter", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Value::List(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(']') => {
                    self.pos += 1;
                    return Ok(Value::List(items));
                }
                Some(',') => {
                    let comma = self.pos;
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(']') {
                        return Err(self.err("Illegal trailing comma before end of array", comma));
                    }
                }
                _ => return Err(self.err("Expecting ',' delimiter", self.pos)),
            }
        }
    }
}

/// `json.dumps(value)` (compact `", "`/`": "` separators) or
/// `json.dumps(value, indent=n)`, both with `ensure_ascii=True`.
pub fn dumps(value: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    write_value(&mut out, value, indent, 0);
    out
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::BigInt(digits) => out.push_str(digits),
        Value::Float(f) => out.push_str(&json_float(*f)),
        Value::Str(s) => write_string(out, s),
        Value::List(items) if items.is_empty() => out.push_str("[]"),
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                separator(out, indent, level + 1, i == 0);
                write_value(out, item, indent, level + 1);
            }
            close(out, indent, level);
            out.push(']');
        }
        Value::Dict(dict) if dict.is_empty() => out.push_str("{}"),
        Value::Dict(dict) => {
            out.push('{');
            for (i, (key, item)) in dict.iter().enumerate() {
                separator(out, indent, level + 1, i == 0);
                write_string(out, key);
                out.push_str(": ");
                write_value(out, item, indent, level + 1);
            }
            close(out, indent, level);
            out.push('}');
        }
    }
}

fn separator(out: &mut String, indent: Option<usize>, level: usize, first: bool) {
    match indent {
        Some(width) => {
            if !first {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(width * level));
        }
        None if !first => out.push_str(", "),
        None => {}
    }
}

fn close(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * level));
    }
}

fn json_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f == f64::INFINITY {
        "Infinity".into()
    } else if f == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        float_repr(f)
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

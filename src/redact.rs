//! Secret redaction for anything that echoes Vast CLI output.

use std::sync::LazyLock;

use regex::Regex;

use crate::pyjson::Value;

pub const REDACTED: &str = "REDACTED";

// Key names Vast treats as secrets (mirrors vastai's support-bundle SENSITIVE_KEY_RE).
static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)api[_-]?key|token|secret|password|passwd|authorization").unwrap()
});
static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)([\w-]*(?:api[_-]?key|token|secret|password|passwd|authorization)[\w-]*["']?\s*[:=]\s*["']?(?:Bearer\s+)?)[^\s,;'"}\]]+"#,
    )
    .unwrap()
});

/// Every secret-looking field or assignment replaced by REDACTED.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Dict(dict) => Value::Dict(
            dict.iter()
                .map(|(key, item)| {
                    let item = if SECRET_KEY.is_match(key) {
                        Value::str(REDACTED)
                    } else {
                        redact(item)
                    };
                    (key.clone(), item)
                })
                .collect(),
        ),
        // Vast reports env vars as [name, value] pairs, e.g. extra_env.
        Value::List(items) => match items.as_slice() {
            [Value::Str(name), _] if SECRET_KEY.is_match(name) => {
                Value::List(vec![Value::Str(name.clone()), Value::str(REDACTED)])
            }
            _ => Value::List(items.iter().map(redact).collect()),
        },
        Value::Str(s) => Value::Str(redact_text(s)),
        other => other.clone(),
    }
}

/// `redact` for a plain string.
pub fn redact_text(text: &str) -> String {
    SECRET_ASSIGNMENT
        .replace_all(text, format!("${{1}}{REDACTED}"))
        .into_owned()
}

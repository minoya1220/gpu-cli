//! Percent encoding with an explicit provider/path safe set.
pub(crate) fn encode(text: &str, safe: &[u8]) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || safe.contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

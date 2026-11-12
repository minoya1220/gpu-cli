//! Accelerator suffixes are hardware counts, not a count of pods.
pub(crate) fn generation(tpu: &str) -> Option<&str> {
    let (generation, size) = tpu.rsplit_once('-')?;
    if generation.is_empty() || size.parse::<u32>().ok()? == 0 {
        return None;
    }
    Some(family(generation))
}
pub(crate) fn family(name: &str) -> &str {
    if name == "v5litepod" {
        "v5e"
    } else {
        name
    }
}
pub(crate) fn size(tpu: &str) -> Option<u32> {
    tpu.rsplit_once('-')?
        .1
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
}
pub(crate) fn public(tpu: String) -> String {
    tpu.strip_prefix("v5litepod-")
        .map(|n| format!("v5e-{n}"))
        .unwrap_or(tpu)
}
pub(crate) fn generations(types: &[String]) -> Vec<String> {
    let mut values = types
        .iter()
        .filter_map(|t| generation(t).map(String::from))
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values.sort_by_key(|g| {
        ["v5e", "v6e", "v5p", "v4", "v3", "v2"]
            .iter()
            .position(|known| *known == g)
            .unwrap_or(usize::MAX)
    });
    values
}
pub(crate) fn members(types: &[String], wanted: &str) -> Vec<String> {
    let mut result = types
        .iter()
        .filter(|t| generation(t) == Some(wanted))
        .cloned()
        .map(public)
        .collect::<Vec<_>>();
    result.sort_by_key(|t| size(t));
    result.dedup();
    result
}
pub(crate) fn label(tpu: &str) -> String {
    let Some(n) = size(tpu) else {
        return tpu.into();
    };
    match generation(tpu) {
        Some("v5e" | "v6e") => format!("{n} {}", if n == 1 { "chip" } else { "chips" }),
        Some("v2" | "v3" | "v4" | "v5p") if n % 2 == 0 => {
            let chips = n / 2;
            format!(
                "{n} TensorCores ({chips} {})",
                if chips == 1 { "chip" } else { "chips" }
            )
        }
        Some("v2" | "v3" | "v4" | "v5p") => format!("{n} TensorCores"),
        _ => format!("Size {n}"),
    }
}
pub fn canonical(tpu: &str) -> String {
    tpu.strip_prefix("v5e-")
        .map(|n| format!("v5litepod-{n}"))
        .unwrap_or_else(|| tpu.into())
}
pub(crate) fn classify(text: &str) -> Option<&'static str> {
    let s = text
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    if s.contains("v6e") || s.contains("trillium") {
        Some("v6e")
    } else if s.contains("v5enterprise") || s.contains("v5p") {
        Some("v5p")
    } else if s.contains("v5lite") || s.contains("v5e") {
        Some("v5e")
    } else if s.contains("v4") {
        Some("v4")
    } else if s.contains("v3") {
        Some("v3")
    } else if s.contains("v2") {
        Some("v2")
    } else {
        None
    }
}
pub(crate) fn chips(tpu: &str) -> Option<(&'static str, f64)> {
    let (g, n) = tpu.rsplit_once('-')?;
    let g = classify(g)?;
    let n: u32 = n.parse().ok()?;
    if n == 0 {
        return None;
    }
    let n = f64::from(n);
    Some((
        g,
        if matches!(g, "v5e" | "v6e") {
            n
        } else {
            n / 2.0
        },
    ))
}
#[cfg(test)]
mod tests;

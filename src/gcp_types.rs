//! Accelerator suffixes are hardware counts, not a count of pods.
pub(crate) fn generation(tpu: &str) -> Option<&str> {
    let (generation, size) = tpu.rsplit_once('-')?;
    if generation.is_empty() || size.parse::<u32>().ok()? == 0 {
        return None;
    }
    Some(if generation == "v5litepod" {
        "v5e"
    } else {
        generation
    })
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn size_units_aliases_and_order_are_not_guessed() {
        assert_eq!(label("v5e-1"), "1 chip");
        assert_eq!(label("v5litepod-4"), "4 chips");
        assert_eq!(label("v6e-8"), "8 chips");
        for t in ["v2-8", "v3-8", "v4-8", "v5p-8"] {
            assert_eq!(label(t), "8 TensorCores (4 chips)");
        }
        assert_eq!(label("v99-8"), "Size 8");
        for bad in ["v5e-0", "v5e-no", "-1"] {
            assert_eq!(generation(bad), None);
        }
        let mixed = [
            "v2-8",
            "v3-8",
            "v4-8",
            "v5p-8",
            "v6e-1",
            "v5litepod-1",
            "v5e-1",
        ]
        .map(String::from);
        assert_eq!(generations(&mixed), ["v5e", "v6e", "v5p", "v4", "v3", "v2"]);
        assert_eq!(members(&mixed, "v5e"), ["v5e-1"]);
        let types = ["v6e-128", "v6e-4", "v6e-1", "v5p-8"].map(String::from);
        assert_eq!(members(&types, "v6e"), ["v6e-1", "v6e-4", "v6e-128"]);
    }
}

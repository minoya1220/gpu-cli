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

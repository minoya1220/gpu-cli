use super::*;
#[test]
fn embedded_iana_coordinates_parse_minutes_seconds_and_fallback_without_os_paths() {
    let ny = zone_coordinates("America/New_York").unwrap();
    assert!((ny.0 - 40.7142).abs() < 0.01 && (ny.1 + 74.0064).abs() < 0.01);
    let india = zone_coordinates("Asia/Kolkata").unwrap();
    assert!((india.0 - 22.5333).abs() < 0.01 && (india.1 - 88.3667).abs() < 0.01);
    assert_eq!(angle("-1800000", 3, 180.0), Some(-180.0));
    assert_eq!(angle("+900100", 2, 90.0), None);
    assert_eq!(angle("+126000", 2, 90.0), None);
    assert!(zone_coordinates("Unknown/Nowhere").is_none());
    assert_eq!(
        timezone_name(&Env::default().with("TZ", ":Asia/Tokyo")),
        Some("Asia/Tokyo".into())
    );
    let original = vec!["unknown-region-a", "us-east5-b", "us-west1-c"];
    for name in [None, Some("Unknown/Nowhere"), Some("UTC")] {
        let mut zones = original.clone();
        order_from_zone_name(name, &mut zones, |z| z);
        assert_eq!(zones, original);
    }
}
#[test]
fn timezone_distance_order_is_offline_and_unknowns_preserve_fallback() {
    let env = Env::default().with("TZ", "America/Los_Angeles");
    let mut zones = vec![
        "future-region1-a",
        "asia-northeast1-b",
        "us-east5-b",
        "us-west1-c",
    ];
    order(&env, &mut zones, |s| s);
    assert_eq!(
        zones,
        [
            "us-west1-c",
            "us-east5-b",
            "asia-northeast1-b",
            "future-region1-a"
        ]
    );
    let original = zones.clone();
    order(&env.with("TZ", "UTC"), &mut zones, |s| s);
    assert_eq!(zones, original);
    order(&Env::default().with("TZ", "Asia/Tokyo"), &mut zones, |s| s);
    assert_eq!(zones[0], "asia-northeast1-b");
}

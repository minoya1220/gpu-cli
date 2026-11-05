//! Approximate geographic ordering only: timezone is not geolocation or RTT.
//! Region city coordinates follow https://cloud.google.com/about/locations.
use crate::ctx::Env;

pub fn region(zone: &str) -> Option<&str> {
    zone.rsplit_once('-').map(|(region, _)| region)
}
fn region_coordinates(region: &str) -> Option<(f64, f64)> {
    Some(match region {
        "us-west1" => (45.60, -121.18),
        "us-west2" => (34.05, -118.24),
        "us-west3" => (40.76, -111.89),
        "us-west4" => (36.17, -115.14),
        "us-central1" => (41.26, -95.86),
        "us-central2" => (36.30, -95.31),
        "us-east1" => (33.20, -80.01),
        "us-east4" => (39.04, -77.49),
        "us-east5" => (39.96, -82.99),
        "us-south1" => (32.78, -96.80),
        "northamerica-northeast1" => (45.50, -73.57),
        "northamerica-northeast2" => (43.65, -79.38),
        "southamerica-east1" => (-23.55, -46.63),
        "southamerica-west1" => (-33.45, -70.67),
        "europe-west1" => (50.45, 3.82),
        "europe-west2" => (51.51, -0.13),
        "europe-west3" => (50.11, 8.68),
        "europe-west4" => (53.44, 6.84),
        "europe-west6" => (47.38, 8.54),
        "europe-west8" => (45.46, 9.19),
        "europe-west9" => (48.86, 2.35),
        "europe-north1" => (60.57, 27.20),
        "europe-central2" => (52.23, 21.01),
        "asia-east1" => (24.07, 120.56),
        "asia-east2" => (22.32, 114.17),
        "asia-northeast1" => (35.68, 139.69),
        "asia-northeast2" => (34.69, 135.50),
        "asia-northeast3" => (37.57, 126.98),
        "asia-south1" => (19.08, 72.88),
        "asia-south2" => (28.61, 77.21),
        "asia-southeast1" => (1.35, 103.82),
        "asia-southeast2" => (-6.21, 106.85),
        "australia-southeast1" => (-33.87, 151.21),
        "australia-southeast2" => (-37.81, 144.96),
        "me-west1" => (32.09, 34.78),
        "me-central1" => (25.29, 51.53),
        "africa-south1" => (-26.20, 28.05),
        _ => return None,
    })
}
// Public-domain IANA tzdb data, embedded so the mapping does not depend on an
// OS-specific zoneinfo installation. Source: https://data.iana.org/time-zones/tzdb/zone1970.tab
const ZONE1970: &str = include_str!("data/zone1970.tab");

fn angle(s: &str, degrees: usize, maximum: f64) -> Option<f64> {
    if !matches!(s.len(), n if n == degrees+3 || n == degrees+5) {
        return None;
    }
    let sign = match s.as_bytes()[0] {
        b'+' => 1.0,
        b'-' => -1.0,
        _ => return None,
    };
    let digits = &s[1..];
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let d: f64 = digits[..degrees].parse().ok()?;
    let m: f64 = digits[degrees..degrees + 2].parse().ok()?;
    let seconds: f64 = if digits.len() == degrees + 4 {
        digits[degrees + 2..].parse().ok()?
    } else {
        0.0
    };
    let magnitude = d + m / 60.0 + seconds / 3600.0;
    (m < 60.0 && seconds < 60.0 && magnitude <= maximum).then_some(sign * magnitude)
}
fn zone_coordinates(name: &str) -> Option<(f64, f64)> {
    ZONE1970
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let mut fields = line.split('\t');
            fields.next()?;
            let coordinates = fields.next()?;
            if fields.next()? != name {
                return None;
            }
            let split = coordinates
                .char_indices()
                .skip(1)
                .find(|(_, c)| matches!(c, '+' | '-'))?
                .0;
            Some((
                angle(&coordinates[..split], 2, 90.0)?,
                angle(&coordinates[split..], 3, 180.0)?,
            ))
        })
}
fn timezone_name(env: &Env) -> Option<String> {
    // TZ is an explicit, portable override. No per-platform paths or commands:
    // the crate alone handles system discovery when no override was supplied.
    if let Some(tz) = env.get("TZ") {
        return Some(tz.strip_prefix(':').unwrap_or(tz).to_string());
    }
    iana_time_zone::get_timezone().ok()
}
fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat, lon) = ((b.0 - a.0).to_radians(), (b.1 - a.1).to_radians());
    (lat / 2.0).sin().powi(2)
        + a.0.to_radians().cos() * b.0.to_radians().cos() * (lon / 2.0).sin().powi(2)
}
pub fn order<T>(env: &Env, items: &mut [T], zone: impl Fn(&T) -> &str) {
    let name = timezone_name(env);
    order_from_zone_name(name.as_deref(), items, zone);
}
fn order_from_zone_name<T>(name: Option<&str>, items: &mut [T], zone: impl Fn(&T) -> &str) {
    let Some(origin) = name.and_then(zone_coordinates) else {
        return;
    };
    items.sort_by(|a, b| {
        let score = |v: &T| {
            region(zone(v))
                .and_then(region_coordinates)
                .map(|p| distance(origin, p))
                .unwrap_or(f64::INFINITY)
        };
        score(a).total_cmp(&score(b))
    });
}
#[cfg(test)]
mod tests {
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
}

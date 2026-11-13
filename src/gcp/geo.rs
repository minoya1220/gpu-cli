//! Approximate geographic ordering only: timezone is not geolocation or RTT.
//! Region city coordinates follow https://cloud.google.com/about/locations.
use crate::ctx::Env;

pub fn region(zone: &str) -> Option<&str> {
    zone.rsplit_once('-').map(|(region, _)| region)
}
const REGIONS: &str = include_str!("data/gcp_region_locations.csv");
pub(crate) fn region_coordinates(region: &str) -> Option<(f64, f64)> {
    let row = REGIONS
        .lines()
        .filter(|line| !line.starts_with('#'))
        .skip(1)
        .find(|line| line.split(',').next() == Some(region))?;
    let mut fields = row.split(',').skip(1).map(str::parse::<f64>);
    let (lat, lon) = (fields.next()?.ok()?, fields.next()?.ok()?);
    (lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0)
        .then_some((lat, lon))
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
mod tests;

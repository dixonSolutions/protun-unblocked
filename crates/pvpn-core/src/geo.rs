//! Approximate the user's position from the system timezone, and measure
//! great-circle distance. Port of the "geography" section of
//! `lib/best-server.py`.
//!
//! `zone.tab` maps every IANA timezone to the coordinates of its reference
//! city — accurate to a few hundred kilometres, ample for ranking servers
//! continents apart, and it costs no network request. That's the point: it
//! has to work on exactly the networks where a geolocation API would be
//! blocked.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const EARTH_RADIUS_KM: f64 = 6371.0;

fn zone_tab_path() -> PathBuf {
    PathBuf::from("/usr/share/zoneinfo/zone.tab")
}

fn localtime_link() -> PathBuf {
    PathBuf::from("/etc/localtime")
}

fn timezone_file() -> PathBuf {
    PathBuf::from("/etc/timezone")
}

/// Return the IANA timezone name, e.g. `Australia/Sydney`.
pub fn local_timezone_name() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        if tz.contains('/') {
            return Some(tz);
        }
    }

    let link = localtime_link();
    if link.exists() {
        if let Ok(resolved) = std::fs::canonicalize(&link) {
            let resolved = resolved.to_string_lossy();
            let marker = "/zoneinfo/";
            if let Some(index) = resolved.find(marker) {
                return Some(resolved[index + marker.len()..].to_string());
            }
        }
    }

    let tzfile = timezone_file();
    if tzfile.exists() {
        if let Ok(text) = std::fs::read_to_string(&tzfile) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

/// Approximate the user's position from the system timezone.
pub fn local_coordinates() -> Option<(f64, f64)> {
    local_coordinates_from(&zone_tab_path())
}

pub fn local_coordinates_from(zone_tab: &Path) -> Option<(f64, f64)> {
    let zone = local_timezone_name()?;
    if !zone_tab.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(zone_tab).ok()?;
    for line in contents.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 3 && fields[2] == zone {
            return parse_iso6709(fields[1]);
        }
    }
    None
}

fn iso6709_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([+-])(\d{2})(\d{2})(\d{2})?([+-])(\d{3})(\d{2})(\d{2})?$").unwrap()
    })
}

/// Parse zone.tab's `±DDMM±DDDMM` / `±DDMMSS±DDDMMSS` coordinates.
pub fn parse_iso6709(text: &str) -> Option<(f64, f64)> {
    let captures = iso6709_regex().captures(text.trim())?;
    let lat_sign = &captures[1];
    let lat_d: i64 = captures[2].parse().ok()?;
    let lat_m: i64 = captures[3].parse().ok()?;
    let lat_s: Option<i64> = captures.get(4).map(|m| m.as_str().parse().ok()).flatten();
    let lon_sign = &captures[5];
    let lon_d: i64 = captures[6].parse().ok()?;
    let lon_m: i64 = captures[7].parse().ok()?;
    let lon_s: Option<i64> = captures.get(8).map(|m| m.as_str().parse().ok()).flatten();

    let latitude = to_degrees(lat_sign, lat_d, lat_m, lat_s);
    let longitude = to_degrees(lon_sign, lon_d, lon_m, lon_s);
    Some((latitude, longitude))
}

fn to_degrees(sign: &str, degrees: i64, minutes: i64, seconds: Option<i64>) -> f64 {
    let value = degrees as f64 + minutes as f64 / 60.0 + seconds.unwrap_or(0) as f64 / 3600.0;
    if sign == "-" {
        -value
    } else {
        value
    }
}

/// Great-circle distance in kilometres between two lat/long pairs.
pub fn haversine_km(origin: (f64, f64), target: (f64, f64)) -> f64 {
    let (lat1, lon1) = (origin.0.to_radians(), origin.1.to_radians());
    let (lat2, lon2) = (target.0.to_radians(), target.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYDNEY: (f64, f64) = (-33.87, 151.22);
    const SINGAPORE: (f64, f64) = (1.29, 103.85);
    const AMSTERDAM: (f64, f64) = (52.37, 4.89);

    #[test]
    fn parses_degrees_and_minutes() {
        let (lat, lon) = parse_iso6709("-3352+15113").unwrap();
        assert!((lat - -(33.0 + 52.0 / 60.0)).abs() < 1e-6);
        assert!((lon - (151.0 + 13.0 / 60.0)).abs() < 1e-6);
    }

    #[test]
    fn parses_seconds_when_present() {
        let (lat, lon) = parse_iso6709("+523722+0045409").unwrap();
        assert!((lat - (52.0 + 37.0 / 60.0 + 22.0 / 3600.0)).abs() < 1e-6);
        assert!((lon - (4.0 + 54.0 / 60.0 + 9.0 / 3600.0)).abs() < 1e-6);
    }

    #[test]
    fn keeps_western_and_southern_signs() {
        let (lat, lon) = parse_iso6709("-2333-04653").unwrap();
        assert!(lat < 0.0);
        assert!(lon < 0.0);
    }

    #[test]
    fn rejects_malformed_input() {
        for text in ["", "nonsense", "3352+15113", "-33+151"] {
            assert!(parse_iso6709(text).is_none(), "expected None for {text:?}");
        }
    }

    #[test]
    fn known_distance_is_close() {
        let distance = haversine_km(SYDNEY, SINGAPORE);
        assert!((distance - 6300.0).abs() < 100.0);
    }

    #[test]
    fn zero_for_same_point() {
        assert!(haversine_km(SYDNEY, SYDNEY).abs() < 1e-6);
    }

    #[test]
    fn symmetric() {
        let there = haversine_km(SYDNEY, AMSTERDAM);
        let back = haversine_km(AMSTERDAM, SYDNEY);
        assert!((there - back).abs() < 1e-6);
    }

    #[test]
    fn orders_singapore_nearer_than_amsterdam() {
        assert!(haversine_km(SYDNEY, SINGAPORE) < haversine_km(SYDNEY, AMSTERDAM));
    }
}

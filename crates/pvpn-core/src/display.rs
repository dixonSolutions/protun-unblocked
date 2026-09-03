//! Human- and machine-readable renderings of a ranked list. Column
//! layout matches `lib/best-server.py` so a `--quick` run can be
//! diffed against the Python helper during the migration.

use crate::rank::{Candidate, WEIGHT_DISTANCE, WEIGHT_LATENCY, WEIGHT_LOAD};

fn thousands(n: f64) -> String {
    let rounded = n.round() as i64;
    let sign = if rounded < 0 { "-" } else { "" };
    let digits = rounded.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    format!("{sign}{}", out.chars().rev().collect::<String>())
}

/// Human-readable ranking, best first. Same columns as the Python table.
pub fn render_table(results: &[Candidate], origin: Option<(f64, f64)>) -> String {
    if results.is_empty() {
        return "No servers matched.".to_string();
    }

    let header = format!(
        "{:>2}  {:<14} {:<22} {:>8} {:>5} {:>8}  RATING",
        "#", "SERVER", "LOCATION", "PING", "LOAD", "DIST"
    );
    let mut lines = vec![header.clone(), "-".repeat(header.len())];

    for (position, candidate) in results.iter().enumerate() {
        let location = if candidate.city.is_empty() {
            candidate.country.clone()
        } else {
            format!("{}, {}", candidate.city, candidate.country)
        };
        let ping = match candidate.latency_ms {
            Some(ms) => format!("{:.0} ms", ms),
            None => "--".to_string(),
        };
        let distance = match candidate.distance_km {
            Some(km) => format!("{} km", thousands(km)),
            None => "--".to_string(),
        };
        let rating = match candidate.rating {
            Some(r) => format!("{r:.1}"),
            None => "--".to_string(),
        };
        lines.push(format!(
            "{:>2}  {:<14} {:<22} {:>8} {:>4}% {:>8}  {rating}",
            position + 1,
            candidate.name,
            location,
            ping,
            candidate.load,
            distance
        ));
    }

    if let Some((lat, lon)) = origin {
        lines.push(String::new());
        lines.push(format!(
            "Distances measured from {lat:.2}, {lon:.2} (system timezone)."
        ));
    }
    lines.join("\n")
}

pub fn render_json(results: &[Candidate], origin: Option<(f64, f64)>) -> anyhow::Result<String> {
    let payload = serde_json::json!({
        "origin": origin.map(|(lat, long)| serde_json::json!({"lat": lat, "long": long})),
        "weights": {
            "latency": WEIGHT_LATENCY,
            "load": WEIGHT_LOAD,
            "distance": WEIGHT_DISTANCE,
        },
        "results": results,
    });
    Ok(serde_json::to_string_pretty(&payload)?)
}

/// One row per server for shell callers.
/// Columns: name, latency ms, load %, distance km, rating, ranking basis.
pub fn render_tsv(results: &[Candidate], measured: bool) -> String {
    let basis = if measured { "measured" } else { "distance" };
    results
        .iter()
        .map(|c| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{basis}",
                c.name,
                c.latency_ms
                    .map(|ms| format!("{ms:.0}"))
                    .unwrap_or_default(),
                c.load,
                c.distance_km
                    .map(|km| format!("{km:.0}"))
                    .unwrap_or_default(),
                c.rating.map(|r| format!("{r:.1}")).unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_names(results: &[Candidate]) -> String {
    results
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rank::Candidate;

    fn candidate(
        name: &str,
        city: &str,
        country: &str,
        latency: Option<f64>,
        load: i64,
        distance: Option<f64>,
    ) -> Candidate {
        Candidate {
            name: name.to_string(),
            entry_ip: "203.0.113.1".to_string(),
            country: country.to_string(),
            city: city.to_string(),
            tier: 0,
            load,
            proton_score: 1.0,
            distance_km: distance,
            latency_ms: latency,
            rating: Some(100.0),
            carry: None,
        }
    }

    #[test]
    fn table_contains_the_rating_header_and_a_server() {
        let text = render_table(
            &[candidate(
                "SG-FREE#2",
                "Singapore",
                "SG",
                None,
                77,
                Some(6300.0),
            )],
            None,
        );
        assert!(text.contains("RATING"));
        assert!(text.contains("SG-FREE#2"));
        assert!(text.contains("Singapore, SG"));
    }

    #[test]
    fn json_is_an_object_with_results() {
        let text = render_json(
            &[candidate("SG-FREE#2", "Singapore", "SG", None, 77, None)],
            None,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("results").is_some());
        assert!(value.get("weights").is_some());
    }
}

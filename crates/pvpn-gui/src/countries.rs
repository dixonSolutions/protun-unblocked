//! Proton's whole server catalogue, the way Proton's own app presents it:
//! by country and city, with features (P2P, Secure Core, Tor, streaming)
//! and what the plan allows.
//!
//! Picking is done here and connecting by name through `pvpn hop`, so a
//! "fastest P2P server in Italy" goes through the same guarded connect as
//! everything else rather than around it.

use pvpn_core::serverlist::RawLogical;
use std::collections::HashMap;
use std::sync::OnceLock;

pub const SECURE_CORE: u32 = 1;
pub const TOR: u32 = 2;
pub const P2P: u32 = 4;
pub const STREAMING: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Any,
    P2p,
    SecureCore,
    Tor,
    Streaming,
}

impl Feature {
    pub fn bit(self) -> u32 {
        match self {
            Feature::Any => 0,
            Feature::P2p => P2P,
            Feature::SecureCore => SECURE_CORE,
            Feature::Tor => TOR,
            Feature::Streaming => STREAMING,
        }
    }

    /// Plain servers exclude Secure Core and Tor, as Proton's app does:
    /// both are slower by design and only wanted on purpose.
    pub fn admits(self, features: u32) -> bool {
        match self {
            Feature::Any => features & (SECURE_CORE | TOR) == 0,
            f => features & f.bit() != 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub country: String,
    pub city: String,
    pub tier: i64,
    pub load: i64,
    /// Proton's own score: lower is better.
    pub score: f64,
    pub features: u32,
    pub online: bool,
}

impl Server {
    pub fn usable(&self, max_tier: i64) -> bool {
        self.online && self.tier <= max_tier
    }

    pub fn feature_labels(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.features & SECURE_CORE != 0 {
            v.push("Secure Core");
        }
        if self.features & P2P != 0 {
            v.push("P2P");
        }
        if self.features & TOR != 0 {
            v.push("Tor");
        }
        if self.features & STREAMING != 0 {
            v.push("Streaming");
        }
        v
    }
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub max_tier: i64,
    pub servers: Vec<Server>,
}

pub fn from_raw(max_tier: i64, raw: Vec<RawLogical>) -> Catalog {
    let servers = raw
        .into_iter()
        .filter_map(|l| {
            let online = l.status.unwrap_or(1) == 1
                && (l.servers.is_empty() || l.servers.iter().any(|p| p.status.unwrap_or(1) == 1));
            Some(Server {
                country: normalise_code(&l.exit_country.clone()?),
                name: l.name?,
                city: l.city.unwrap_or_default(),
                tier: l.tier.unwrap_or(0),
                load: l.load.unwrap_or(0),
                score: l.score.unwrap_or(f64::MAX),
                features: l.features.unwrap_or(0) as u32,
                online,
            })
        })
        .collect();
    Catalog { max_tier, servers }
}

pub fn load() -> Catalog {
    match pvpn_core::serverlist::load_server_list(&pvpn_core::paths::serverlist_path()) {
        Ok((max_tier, raw)) => from_raw(max_tier, raw),
        Err(_) => Catalog::default(),
    }
}

fn normalise_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

impl Catalog {
    /// Countries with at least one server passing `feature`: code, servers,
    /// usable on this plan. The ones this plan can use first, each group by
    /// name — a free account should not open on a wall of locked rows.
    pub fn countries(&self, feature: Feature) -> Vec<(String, usize, usize)> {
        let mut map: HashMap<&str, (usize, usize)> = HashMap::new();
        for s in self.servers.iter().filter(|s| feature.admits(s.features)) {
            let e = map.entry(&s.country).or_default();
            e.0 += 1;
            if s.usable(self.max_tier) {
                e.1 += 1;
            }
        }
        let mut v: Vec<(String, usize, usize)> = map.into_iter().map(|(c, (n, u))| (c.to_string(), n, u)).collect();
        v.sort_by_key(|(c, _, u)| (*u == 0, country_name(c)));
        v
    }

    pub fn in_country<'a>(&'a self, country: &'a str, feature: Feature, city: Option<&'a str>) -> impl Iterator<Item = &'a Server> + 'a {
        self.servers.iter().filter(move |s| {
            s.country == country && feature.admits(s.features) && city.is_none_or(|c| s.city == c)
        })
    }

    pub fn cities(&self, country: &str, feature: Feature) -> Vec<String> {
        let mut v: Vec<String> = self.in_country(country, feature, None).map(|s| s.city.clone()).filter(|c| !c.is_empty()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Proton's pick among `candidates`: usable on this plan, lowest score.
    pub fn fastest<'a>(&self, candidates: impl Iterator<Item = &'a Server>) -> Option<&'a Server> {
        candidates
            .filter(|s| s.usable(self.max_tier))
            .min_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
    }

    pub fn random<'a>(&self, candidates: impl Iterator<Item = &'a Server>) -> Option<&'a Server> {
        let usable: Vec<&Server> = candidates.filter(|s| s.usable(self.max_tier)).collect();
        if usable.is_empty() {
            return None;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize ^ d.as_secs() as usize)
            .unwrap_or(0);
        Some(usable[nanos % usable.len()])
    }
}

/// English country name from the system's iso-codes, with Proton's
/// non-ISO `UK` handled; the code itself when neither knows it.
pub fn country_name(code: &str) -> String {
    static NAMES: OnceLock<HashMap<String, String>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        let mut m = HashMap::new();
        if let Ok(text) = std::fs::read_to_string("/usr/share/iso-codes/json/iso_3166-1.json") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                for e in v.get("3166-1").and_then(|l| l.as_array()).into_iter().flatten() {
                    let code = e.get("alpha_2").and_then(|c| c.as_str());
                    let name = e.get("common_name").or_else(|| e.get("name")).and_then(|n| n.as_str());
                    if let (Some(c), Some(n)) = (code, name) {
                        m.insert(c.to_string(), n.to_string());
                    }
                }
            }
        }
        m
    });
    let iso = if code == "UK" { "GB" } else { code };
    names.get(iso).cloned().unwrap_or_else(|| code.to_string())
}

/// 🇯🇵 from `JP`.
pub fn flag(code: &str) -> String {
    let iso = if code == "UK" { "GB" } else { code };
    if iso.len() != 2 || !iso.chars().all(|c| c.is_ascii_alphabetic()) {
        return String::new();
    }
    iso.to_ascii_uppercase()
        .chars()
        .filter_map(|c| char::from_u32(0x1F1E6 + (c as u32 - 'A' as u32)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, country: &str, city: &str, tier: i64, score: f64, features: u32, online: bool) -> Server {
        Server { name: name.into(), country: country.into(), city: city.into(), tier, load: 10, score, features, online }
    }

    fn catalog() -> Catalog {
        Catalog {
            max_tier: 0,
            servers: vec![
                s("JP-FREE#1", "JP", "Tokyo", 0, 3.0, 0, true),
                s("JP-FREE#2", "JP", "Osaka", 0, 2.0, 0, true),
                s("JP#9", "JP", "Tokyo", 2, 0.5, P2P, true),
                s("CH-JP#1", "JP", "Tokyo", 2, 0.1, SECURE_CORE, true),
                s("NL-FREE#3", "NL", "Amsterdam", 0, 1.0, 0, false),
            ],
        }
    }

    #[test]
    fn fastest_respects_plan_status_and_score() {
        let c = catalog();
        assert_eq!(c.fastest(c.in_country("JP", Feature::Any, None)).unwrap().name, "JP-FREE#2");
        assert_eq!(c.fastest(c.in_country("JP", Feature::Any, Some("Tokyo"))).unwrap().name, "JP-FREE#1");
        assert!(c.fastest(c.in_country("NL", Feature::Any, None)).is_none(), "offline");
        assert!(c.fastest(c.in_country("JP", Feature::P2p, None)).is_none(), "Plus only");
    }

    #[test]
    fn plain_listings_leave_out_secure_core_and_tor() {
        let c = catalog();
        let names: Vec<&str> = c.in_country("JP", Feature::Any, None).map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"CH-JP#1"));
        assert_eq!(c.in_country("JP", Feature::SecureCore, None).count(), 1);
        let all = c.countries(Feature::Any);
        let jp = all.iter().find(|x| x.0 == "JP").unwrap();
        assert_eq!((jp.1, jp.2), (3, 2));
        assert_eq!(all.last().unwrap().0, "NL", "nothing usable sorts last");
    }

    #[test]
    fn flags_and_proton_codes() {
        assert_eq!(flag("JP"), "🇯🇵");
        assert_eq!(flag("UK"), "🇬🇧");
        assert_eq!(flag("??"), "");
    }
}

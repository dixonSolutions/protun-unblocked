//! Score and sort candidates. Port of the "ranking" section of
//! `lib/best-server.py` — same weights, same middlebox-detection guard,
//! same reasoning. See `docs/best-server.md` for the why.

use serde::{Deserialize, Serialize};

/// A server feature bit flag, mirroring `proton.vpn.session.servers.types`.
pub const FEATURE_SECURE_CORE: u32 = 1 << 0;
pub const FEATURE_TOR: u32 = 1 << 1;
/// Secure Core doubles your latency by design and Tor servers are slower
/// still; neither belongs in a ranking whose whole purpose is speed.
pub const FEATURES_EXCLUDED_BY_DEFAULT: u32 = FEATURE_SECURE_CORE | FEATURE_TOR;

pub const STATUS_ENABLED: i64 = 1;

/// How the final rating is composed. Latency dominates because it is the
/// only term that reflects this network rather than a prediction about it.
pub const WEIGHT_LATENCY: f64 = 0.60;
pub const WEIGHT_LOAD: f64 = 0.25;
pub const WEIGHT_DISTANCE: f64 = 0.15;

/// A server this loaded is treated as fully saturated when scoring, so the
/// difference between 95% and 99% does not swamp a real latency advantage.
pub const LOAD_SATURATION_PERCENT: f64 = 95.0;

/// Latency is scored against the *fastest server in the pool*, not against
/// the pool's min-max range.
///
/// The range is the trap. A shortlist is mostly filler — half of ours came
/// back Amsterdam, because free NL servers are the least loaded and the
/// shortlist reserves half its slots for those. Amsterdam is 16,000 km and
/// ~600 ms away, so it set `latency_high`, and every real contender then
/// got squeezed into the bottom tenth of the scale: 206 ms vs 285 ms
/// scored 0.004 vs 0.112 apart. Load, whose range *was* set by contenders,
/// kept its full span and quietly decided the ranking — which is how a
/// Sydney client got handed Los Angeles over Singapore. The weights below
/// only mean what they say if no term's scale depends on servers nobody
/// would pick.
///
/// A ratio also travels between networks in a way a range does not: this
/// tool exists for links where a middlebox adds a few hundred milliseconds
/// to *every* handshake, and only the ratio between servers survives that.
/// Twice the latency of the best is as bad as this term gets.
pub const LATENCY_SATURATION_RATIO: f64 = 2.0;

/// Distance, by contrast, is absolute — a kilometre is a kilometre on any
/// network — so it is scored against a fixed saturation point rather than
/// whatever the pool happens to contain. Roughly the far side of the
/// planet by cable.
pub const DISTANCE_SATURATION_KM: f64 = 15_000.0;

/// When measurements cannot be trusted, latency's share goes to distance,
/// the only honest predictor left.
pub const WEIGHT_DISTANCE_UNMEASURED: f64 = WEIGHT_LATENCY + WEIGHT_DISTANCE;

/// A handshake this fast cannot have crossed an ocean, so if every server
/// looks this good while some are continents away, we are timing a
/// middlebox and the numbers mean nothing.
pub const IMPLAUSIBLE_LATENCY_MS: f64 = 20.0;
pub const NEARBY_KM: f64 = 1000.0;

/// One logical server, plus everything measured or derived about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub name: String,
    pub entry_ip: String,
    pub country: String,
    pub city: String,
    pub tier: i64,
    pub load: i64,
    pub proton_score: f64,
    pub distance_km: Option<f64>,
    pub latency_ms: Option<f64>,
    pub rating: Option<f64>,
    /// What real connects on this network have taught us about this server.
    /// `None` where there is no history to consult — `pvpn best
    /// --serverlist` and the tests — which ranks exactly as it always did.
    #[serde(default)]
    pub carry: Option<CarryRecord>,
}

/// Real connect attempts on this network, and how many of them carried
/// traffic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarryRecord {
    pub attempts: u32,
    pub successes: u32,
}

/// How much a server's own connect history is worth against its handshake.
///
/// Not a weight in the rating — a tier above it. A latency probe opens a TCP
/// connection and stops there, which on the networks this tool exists for is
/// precisely the part that always works: the middlebox completes the
/// handshake and then kills the session behind it. Scoring the two together
/// lets 40ms of ping outvote twenty-one connects that actually carried
/// traffic, which is what picked SG-FREE#13 (0 successes in 3 attempts,
/// 205ms) over SG-FREE#2 (21 successes in 30, 244ms) every time on
/// 2026-09-02.
///
/// So they are ordered, not blended. Within a tier the existing rating
/// decides, untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CarryTier {
    /// Has carried traffic here.
    Proven,
    /// Never tried here. The honest default, and where every server starts.
    Unknown,
    /// Tried here, repeatedly, and never once carried.
    Failing,
}

/// One failed connect is noise — a server restarting, a roam, a bad moment.
/// A pattern needs more than that before a server is ranked below one
/// nobody has tried.
const FAILURES_BEFORE_DEMOTION: u32 = 2;

fn carry_tier(carry: Option<CarryRecord>) -> CarryTier {
    match carry {
        Some(record) if record.successes > 0 => CarryTier::Proven,
        Some(record) if record.attempts >= FAILURES_BEFORE_DEMOTION => CarryTier::Failing,
        _ => CarryTier::Unknown,
    }
}

impl Candidate {
    pub fn reachable(&self) -> bool {
        self.latency_ms.is_some()
    }
}

/// Cut the pool down to the servers worth spending a probe on.
///
/// Probing all 100+ free servers would work but is wasteful, so this keeps
/// the nearest ones (distance predicts latency well) and the least loaded
/// ones, interleaved so neither criterion monopolises the shortlist.
pub fn shortlist(candidates: &[Candidate], limit: usize) -> Vec<Candidate> {
    if limit == 0 || candidates.len() <= limit {
        return candidates.to_vec();
    }

    let have_distance = candidates.iter().any(|c| c.distance_km.is_some());

    let mut primary: Vec<&Candidate> = candidates.iter().collect();
    if have_distance {
        primary.sort_by(|a, b| {
            (a.distance_km.is_none(), a.distance_km.unwrap_or(0.0))
                .partial_cmp(&(b.distance_km.is_none(), b.distance_km.unwrap_or(0.0)))
                .unwrap()
        });
    } else {
        // No idea where we are: fall back to Proton's own ordering.
        primary.sort_by(|a, b| a.proton_score.partial_cmp(&b.proton_score).unwrap());
    }

    let mut by_load: Vec<&Candidate> = candidates.iter().collect();
    by_load.sort_by_key(|c| c.load);

    let mut picked: Vec<(String, Candidate)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    'outer: for index in 0..candidates.len() {
        for candidate in [primary[index], by_load[index]] {
            if picked.len() >= limit {
                break 'outer;
            }
            if seen.insert(candidate.name.clone()) {
                picked.push((candidate.name.clone(), candidate.clone()));
            }
        }
    }
    picked.into_iter().map(|(_, c)| c).collect()
}

/// Do the measured latencies describe the path, or a middlebox?
///
/// A transparent proxy answers on behalf of every destination, so every
/// server comes back a couple of milliseconds away. If nothing measured is
/// plausibly far away while some servers demonstrably are, the measurements
/// are not about distance.
pub fn latency_is_informative(candidates: &[Candidate]) -> bool {
    let reachable: Vec<&Candidate> = candidates.iter().filter(|c| c.reachable()).collect();
    if reachable.is_empty() {
        return false;
    }
    let max_latency = reachable
        .iter()
        .map(|c| c.latency_ms.unwrap())
        .fold(f64::MIN, f64::max);
    if max_latency >= IMPLAUSIBLE_LATENCY_MS {
        return true;
    }
    let distances: Vec<f64> = reachable.iter().filter_map(|c| c.distance_km).collect();
    if distances.is_empty() {
        return true; // no geography to contradict them; take them as read
    }
    distances.iter().cloned().fold(f64::MIN, f64::max) < NEARBY_KM
}

/// How much worse than the fastest server this one is, on a 0..1 scale
/// that saturates at [`LATENCY_SATURATION_RATIO`] times the best.
fn latency_penalty(latency_ms: f64, best_ms: f64) -> f64 {
    if best_ms <= 0.0 {
        return 0.0;
    }
    let excess = latency_ms / best_ms - 1.0;
    (excess / (LATENCY_SATURATION_RATIO - 1.0)).clamp(0.0, 1.0)
}

/// Load is already a percentage, so it needs no pool at all: it is scored
/// straight against the saturation point.
fn load_penalty(load: f64) -> f64 {
    (load / LOAD_SATURATION_PERCENT).clamp(0.0, 1.0)
}

fn distance_penalty(km: f64) -> f64 {
    (km / DISTANCE_SATURATION_KM).clamp(0.0, 1.0)
}

/// Score and sort candidates, best first.
///
/// Every term is scored against a fixed saturation point — a ratio for
/// latency, absolute scales for load and distance — so a rating means the
/// same thing from one run to the next and cannot be moved by which
/// also-rans happened to share the pool. See [`LATENCY_SATURATION_RATIO`]
/// for what went wrong when the terms were normalised against the pool's
/// own range instead.
///
/// 100 is now an ideal server (fastest measured, idle, next door) rather
/// than merely the best of whatever was probed, so ratings are comparable
/// across runs and across networks.
pub fn rank(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut reachable: Vec<Candidate> = candidates
        .iter()
        .filter(|c| c.reachable())
        .cloned()
        .collect();
    let mut unreachable: Vec<Candidate> = candidates
        .iter()
        .filter(|c| !c.reachable())
        .cloned()
        .collect();

    let measured = latency_is_informative(candidates);
    let weight_latency = if measured { WEIGHT_LATENCY } else { 0.0 };
    let weight_distance = if measured {
        WEIGHT_DISTANCE
    } else {
        WEIGHT_DISTANCE_UNMEASURED
    };

    if !reachable.is_empty() {
        // The fastest handshake in the pool is the reference. It is the one
        // statistic here that outliers cannot move: the noise on these
        // links is one-sided — a middlebox, a queue or a retransmit only
        // ever *adds* time — which is the same reason `probe_candidate`
        // keeps the minimum of its samples rather than the mean.
        let best_latency = reachable
            .iter()
            .map(|c| c.latency_ms.unwrap())
            .fold(f64::MAX, f64::min);

        for candidate in reachable.iter_mut() {
            let mut cost = weight_latency
                * latency_penalty(candidate.latency_ms.unwrap(), best_latency)
                + WEIGHT_LOAD * load_penalty(candidate.load as f64);
            if let Some(distance) = candidate.distance_km {
                cost += weight_distance * distance_penalty(distance);
            }
            candidate.rating = Some((100.0 * (1.0 - cost) * 10.0).round() / 10.0);
        }
    }

    for candidate in unreachable.iter_mut() {
        candidate.rating = None;
    }

    if measured {
        reachable.sort_by(|a, b| {
            let ra = -(a.rating.unwrap_or(0.0));
            let rb = -(b.rating.unwrap_or(0.0));
            carry_tier(a.carry).cmp(&carry_tier(b.carry)).then(
                ra.partial_cmp(&rb).unwrap().then(
                    a.latency_ms
                        .unwrap()
                        .partial_cmp(&b.latency_ms.unwrap())
                        .unwrap(),
                ),
            )
        });
    } else {
        reachable.sort_by(|a, b| {
            let ra = -(a.rating.unwrap_or(0.0));
            let rb = -(b.rating.unwrap_or(0.0));
            let da = a.distance_km.unwrap_or(f64::INFINITY);
            let db = b.distance_km.unwrap_or(f64::INFINITY);
            // Latency could not be trusted here, which makes the carry
            // record the *only* measurement of this network in the sort.
            carry_tier(a.carry).cmp(&carry_tier(b.carry)).then(
                ra.partial_cmp(&rb)
                    .unwrap()
                    .then(da.partial_cmp(&db).unwrap()),
            )
        });
    }
    unreachable.sort_by(|a, b| {
        let da = a.distance_km.unwrap_or(f64::INFINITY);
        let db = b.distance_km.unwrap_or(f64::INFINITY);
        da.partial_cmp(&db).unwrap()
    });

    reachable.into_iter().chain(unreachable).collect()
}

/// Order by prediction alone, for when probing is turned off. Distance
/// stands in for latency; Proton's score breaks ties. Strictly worse than
/// measuring, and exists only so a no-probe run still returns something.
pub fn rank_without_probing(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut ordered: Vec<Candidate> = candidates.to_vec();
    ordered.sort_by(|a, b| {
        let da = a.distance_km.unwrap_or(f64::INFINITY);
        let db = b.distance_km.unwrap_or(f64::INFINITY);
        carry_tier(a.carry).cmp(&carry_tier(b.carry)).then(
            da.partial_cmp(&db)
                .unwrap()
                .then(a.load.cmp(&b.load))
                .then(a.proton_score.partial_cmp(&b.proton_score).unwrap()),
        )
    });
    for candidate in ordered.iter_mut() {
        candidate.rating = None;
    }
    ordered
}

#[cfg(test)]
mod carry_tests {
    use super::*;

    fn candidate(name: &str, latency: f64, carry: Option<CarryRecord>) -> Candidate {
        Candidate {
            name: name.to_string(),
            entry_ip: "203.0.113.1".to_string(),
            country: "SG".to_string(),
            city: "Singapore".to_string(),
            tier: 0,
            load: 50,
            proton_score: 1.0,
            distance_km: Some(6300.0),
            latency_ms: Some(latency),
            rating: None,
            carry: Some(carry.unwrap_or_default()),
        }
    }

    #[test]
    fn a_server_that_has_carried_traffic_outranks_a_faster_one_that_never_has() {
        // The 2026-09-02 pick, exactly: SG-FREE#13 pinged 205ms with 0
        // successes in 3 attempts and won every time over SG-FREE#2 at
        // 244ms with 21 successes in 30. A probe opens a TCP connection and
        // stops, which on this network is the half that always works.
        let ranked = rank(&[
            candidate(
                "SG-FREE#13",
                204.8,
                Some(CarryRecord {
                    attempts: 3,
                    successes: 0,
                }),
            ),
            candidate(
                "SG-FREE#2",
                244.1,
                Some(CarryRecord {
                    attempts: 30,
                    successes: 21,
                }),
            ),
        ]);
        assert_eq!(ranked[0].name, "SG-FREE#2");
        assert_eq!(ranked[1].name, "SG-FREE#13");
    }

    #[test]
    fn one_bad_night_does_not_demote_a_server_below_an_untried_one() {
        let ranked = rank(&[
            candidate(
                "A#1",
                300.0,
                Some(CarryRecord {
                    attempts: 1,
                    successes: 0,
                }),
            ),
            candidate("B#1", 200.0, None),
        ]);
        assert_eq!(
            ranked[0].name, "B#1",
            "faster wins on rating; neither is demoted"
        );
        assert_eq!(ranked[1].name, "A#1");
    }

    #[test]
    fn with_no_history_the_ranking_is_exactly_what_it_always_was() {
        let mut fast = candidate("FAST#1", 200.0, None);
        let mut slow = candidate("SLOW#1", 400.0, None);
        fast.carry = None;
        slow.carry = None;
        let ranked = rank(&[slow, fast]);
        assert_eq!(ranked[0].name, "FAST#1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, latency: Option<f64>, load: i64, distance: Option<f64>) -> Candidate {
        Candidate {
            name: name.to_string(),
            entry_ip: "203.0.113.1".to_string(),
            country: "XX".to_string(),
            city: "Nowhere".to_string(),
            tier: 0,
            load,
            proton_score: 1.0,
            distance_km: distance,
            latency_ms: latency,
            rating: None,
            carry: None,
        }
    }

    fn names(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(|c| c.name.clone()).collect()
    }

    #[test]
    fn faster_server_wins_at_equal_load() {
        let slow = candidate("slow", Some(300.0), 50, None);
        let fast = candidate("fast", Some(100.0), 50, None);
        assert_eq!(names(&rank(&[slow, fast])), vec!["fast", "slow"]);
    }

    #[test]
    fn quieter_server_wins_at_equal_latency() {
        let busy = candidate("busy", Some(100.0), 90, None);
        let idle = candidate("idle", Some(100.0), 10, None);
        assert_eq!(names(&rank(&[busy, idle])), vec!["idle", "busy"]);
    }

    #[test]
    fn latency_outweighs_load() {
        let near_busy = candidate("near-busy", Some(100.0), 95, None);
        let far_idle = candidate("far-idle", Some(300.0), 0, None);
        assert_eq!(
            names(&rank(&[far_idle, near_busy])),
            vec!["near-busy", "far-idle"]
        );
    }

    #[test]
    fn unreachable_servers_sort_last() {
        let reachable = candidate("up", Some(250.0), 99, None);
        let unreachable = candidate("down", None, 1, None);
        assert_eq!(names(&rank(&[unreachable, reachable])), vec!["up", "down"]);
    }

    #[test]
    fn unreachable_servers_have_no_rating() {
        let unreachable = candidate("down", None, 50, None);
        assert!(rank(&[unreachable])[0].rating.is_none());
    }

    #[test]
    fn rating_is_a_percentage() {
        let candidates = vec![
            candidate("a", Some(100.0), 10, Some(1000.0)),
            candidate("b", Some(200.0), 50, Some(5000.0)),
            candidate("c", Some(300.0), 90, Some(9000.0)),
        ];
        for result in rank(&candidates) {
            let r = result.rating.unwrap();
            assert!((0.0..=100.0).contains(&r));
        }
    }

    #[test]
    fn a_rating_is_absolute_not_relative_to_the_pool() {
        // 100 means an ideal server, not "the best of whatever we probed".
        // Ranking first in a bad pool must not earn a perfect score, or a
        // rating cannot be compared between two runs.
        let ideal = rank(&[candidate("ideal", Some(100.0), 0, Some(0.0))]);
        assert!(ideal[0].rating.unwrap() > 99.9);

        let best_of_a_bad_lot = rank(&[
            candidate("least-bad", Some(100.0), 90, Some(12_000.0)),
            candidate("worse", Some(400.0), 95, Some(16_000.0)),
        ]);
        assert_eq!(best_of_a_bad_lot[0].name, "least-bad");
        assert!(best_of_a_bad_lot[0].rating.unwrap() < 80.0);
    }

    #[test]
    fn single_candidate_does_not_divide_by_zero() {
        let results = rank(&[candidate("only", Some(123.0), 50, Some(10.0))]);
        let rating = results[0].rating.expect("a reachable server is rated");
        assert!((0.0..=100.0).contains(&rating));
    }

    #[test]
    fn filler_servers_cannot_hand_the_win_to_a_distant_one() {
        // The Los Angeles regression. Half the probed shortlist came back
        // Amsterdam — furthest away, slowest, and least loaded, so it set
        // both the latency and the distance range. Normalised against that,
        // Singapore's 79 ms and 5,800 km advantages nearly vanished and Los
        // Angeles won on load alone. These are the real measurements.
        let pool = vec![
            candidate("SG", Some(206.0), 84, Some(6_300.0)),
            candidate("US", Some(285.0), 53, Some(12_073.0)),
            candidate("NL-a", Some(570.0), 50, Some(16_644.0)),
            candidate("NL-b", Some(646.0), 61, Some(16_644.0)),
        ];
        assert_eq!(names(&rank(&pool))[0], "SG");

        // ...and the filler must not be what decides it: the same two
        // contenders alone rank the same way.
        assert_eq!(names(&rank(&pool[..2]))[0], "SG");
    }

    #[test]
    fn extreme_load_is_capped() {
        let saturated_fast = candidate("fast", Some(100.0), 99, None);
        let saturated_slow = candidate("slow", Some(280.0), 96, None);
        assert_eq!(
            names(&rank(&[saturated_slow, saturated_fast])),
            vec!["fast", "slow"]
        );
    }

    #[test]
    fn handles_an_empty_list() {
        assert!(rank(&[]).is_empty());
    }

    #[test]
    fn rejects_impossibly_fast_intercontinental_replies() {
        let pool = vec![
            candidate("NL", Some(3.1), 50, Some(16600.0)),
            candidate("SG", Some(2.4), 50, Some(6300.0)),
            candidate("JP", Some(2.0), 50, Some(7800.0)),
        ];
        assert!(!latency_is_informative(&pool));
    }

    #[test]
    fn accepts_plausible_measurements() {
        let pool = vec![
            candidate("NL", Some(280.0), 50, Some(16600.0)),
            candidate("SG", Some(99.0), 50, Some(6300.0)),
        ];
        assert!(latency_is_informative(&pool));
    }

    #[test]
    fn fast_replies_are_fine_when_everything_is_nearby() {
        let pool = vec![
            candidate("A", Some(3.0), 50, Some(40.0)),
            candidate("B", Some(2.0), 50, Some(120.0)),
        ];
        assert!(latency_is_informative(&pool));
    }

    #[test]
    fn no_geography_means_the_numbers_stand() {
        let pool = vec![
            candidate("A", Some(2.0), 50, None),
            candidate("B", Some(3.0), 50, None),
        ];
        assert!(latency_is_informative(&pool));
    }

    #[test]
    fn unreachable_only_is_not_informative() {
        assert!(!latency_is_informative(&[candidate("A", None, 50, None)]));
    }

    #[test]
    fn rank_falls_back_to_distance_when_latency_is_noise() {
        let pool = vec![
            candidate("NL", Some(1.7), 50, Some(16600.0)),
            candidate("SG", Some(2.4), 50, Some(6300.0)),
        ];
        assert_eq!(names(&rank(&pool)), vec!["SG", "NL"]);
    }

    #[test]
    fn rank_still_prefers_a_measured_win() {
        let pool = vec![
            candidate("far-fast", Some(90.0), 50, Some(16600.0)),
            candidate("near-slow", Some(400.0), 50, Some(1200.0)),
        ];
        assert_eq!(rank(&pool)[0].name, "far-fast");
    }

    #[test]
    fn rank_without_probing_orders_by_distance_then_load() {
        let far = candidate("far", None, 10, Some(9000.0));
        let near_busy = candidate("near-busy", None, 90, Some(100.0));
        let near_idle = candidate("near-idle", None, 10, Some(100.0));
        let results = rank_without_probing(&[far, near_busy, near_idle]);
        assert_eq!(names(&results), vec!["near-idle", "near-busy", "far"]);
    }

    #[test]
    fn rank_without_probing_servers_without_distance_sort_last() {
        let known = candidate("known", None, 50, Some(9999.0));
        let unknown = candidate("unknown", None, 50, None);
        let results = rank_without_probing(&[unknown, known]);
        assert_eq!(names(&results), vec!["known", "unknown"]);
    }

    #[test]
    fn rank_without_probing_clears_ratings() {
        let mut item = candidate("a", None, 50, Some(1.0));
        item.rating = Some(99.0);
        assert!(rank_without_probing(&[item])[0].rating.is_none());
    }

    #[test]
    fn shortlist_returns_everything_when_under_the_limit() {
        let candidates: Vec<Candidate> = (0..3)
            .map(|i| candidate(&i.to_string(), None, 50, None))
            .collect();
        assert_eq!(shortlist(&candidates, 10).len(), 3);
    }

    #[test]
    fn shortlist_zero_limit_means_no_limit() {
        let candidates: Vec<Candidate> = (0..30)
            .map(|i| candidate(&i.to_string(), None, 50, None))
            .collect();
        assert_eq!(shortlist(&candidates, 0).len(), 30);
    }

    #[test]
    fn shortlist_respects_the_limit() {
        let candidates: Vec<Candidate> = (0..50)
            .map(|i| candidate(&i.to_string(), None, 50, Some(i as f64)))
            .collect();
        assert!(shortlist(&candidates, 10).len() <= 10);
    }

    #[test]
    fn shortlist_keeps_the_nearest_server() {
        let candidates: Vec<Candidate> = (0..50)
            .map(|i| candidate(&i.to_string(), None, 50, Some(i as f64)))
            .collect();
        let names: std::collections::HashSet<String> = shortlist(&candidates, 10)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(names.contains("0"));
    }

    #[test]
    fn shortlist_keeps_the_quietest_server_even_when_distant() {
        let mut candidates: Vec<Candidate> = (0..50)
            .map(|i| candidate(&i.to_string(), None, 90, Some(i as f64)))
            .collect();
        candidates[49].load = 1;
        let names: std::collections::HashSet<String> = shortlist(&candidates, 10)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(
            names.contains("49"),
            "a far but idle server should still be measured"
        );
    }

    #[test]
    fn shortlist_falls_back_to_proton_score_without_distances() {
        let mut candidates: Vec<Candidate> = (0..50)
            .map(|i| candidate(&i.to_string(), None, 50, None))
            .collect();
        for (index, item) in candidates.iter_mut().enumerate() {
            item.proton_score = (50 - index) as f64;
        }
        let names: std::collections::HashSet<String> = shortlist(&candidates, 5)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(
            names.contains("49"),
            "lowest Proton score should survive the cut"
        );
    }
}

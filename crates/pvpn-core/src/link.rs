//! Is *our own uplink* still there?
//!
//! Every other check in this tool asks whether a Proton server works. This
//! one asks the question that has to be answered first, because getting it
//! wrong is expensive in a way that is invisible afterwards: when the wifi
//! drops, roams, or the captive portal reasserts itself mid-connect, the
//! tunnel stops carrying traffic — and a tool that only measures traffic
//! writes that down as "this server is blocked here".
//!
//! Do that three times and the ranked list for this network has three
//! healthy servers retired from it, held for a day each, escalating to four
//! days apiece if it happens again. The next connect then starts from a
//! worse list because the laptop walked out of range once.
//!
//! So: before blaming a server, ask whether the thing under the tunnel is
//! still up. The uplink's own gateway is the right thing to ask, because it
//! is reachable *outside* the tunnel — a full-tunnel route does not disturb
//! the on-link subnet route — so it answers while every other address on
//! the machine is going through a tunnel that may be dead.
//!
//! [`LinkHealth::Unknown`] exists and is used deliberately. A gateway that
//! ignores ICMP with no fresh ARP entry is not evidence of anything, and
//! "assume the network died" is exactly as damaging in the other direction:
//! it would stop `pvpn up` walking the list on networks where every server
//! really is being killed, which is the case this tool exists for. Only
//! `Down` — positive evidence — changes any decision.

use crate::proc::run_with_timeout;
use std::time::Duration;

/// The physical link this machine's traffic leaves by, outside any tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uplink {
    pub device: String,
    pub gateway: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkHealth {
    /// The gateway answered, or the kernel confirmed it recently.
    Up,
    /// Positive evidence the uplink is gone: no connected device, or a
    /// gateway the kernel has given up on.
    Down,
    /// No evidence either way. Never treated as `Down`.
    Unknown,
}

impl LinkHealth {
    /// The only question callers should ask. Deliberately not `!= Up`.
    pub fn is_down(self) -> bool {
        self == LinkHealth::Down
    }
}

/// Devices NetworkManager currently considers a connected physical uplink.
fn connected_uplinks() -> Vec<String> {
    let Ok(result) = run_with_timeout(
        "nmcli",
        &["-t", "-f", "DEVICE,TYPE,STATE", "dev", "status"],
        &[],
        Duration::from_secs(3),
    ) else {
        return Vec::new();
    };
    parse_connected_uplinks(&result.stdout)
}

fn parse_connected_uplinks(dev_status: &str) -> Vec<String> {
    dev_status
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ':');
            let device = parts.next()?;
            let kind = parts.next()?;
            let state = parts.next()?;
            ((kind == "wifi" || kind == "ethernet") && state.starts_with("connected"))
                .then(|| device.to_string())
        })
        .collect()
}

/// The default route that leaves by a physical device.
///
/// Not simply "the default route": while a tunnel is up there are several,
/// and the one with the lowest metric belongs to the tunnel. The physical
/// one survives alongside it, which is the whole reason this check keeps
/// working when the tunnel does not.
pub fn uplink() -> Option<Uplink> {
    let result = run_with_timeout(
        "ip",
        &["-4", "route", "show", "default"],
        &[],
        Duration::from_secs(3),
    )
    .ok()?;
    parse_uplink(&result.stdout, &connected_uplinks())
}

fn parse_uplink(routes: &str, uplinks: &[String]) -> Option<Uplink> {
    routes.lines().find_map(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let gateway = tokens
            .iter()
            .position(|t| *t == "via")
            .and_then(|i| tokens.get(i + 1))?;
        let device = tokens
            .iter()
            .position(|t| *t == "dev")
            .and_then(|i| tokens.get(i + 1))?;
        uplinks.iter().any(|u| u == device).then(|| Uplink {
            device: device.to_string(),
            gateway: gateway.to_string(),
        })
    })
}

/// One ICMP echo, bound to the uplink device so the tunnel's routes cannot
/// claim it. One second: this runs inside a poll loop, and a gateway that
/// needs longer than that is not the fast answer we came for.
fn gateway_answers(link: &Uplink) -> bool {
    run_with_timeout(
        "ping",
        &[
            "-n",
            "-c",
            "1",
            "-W",
            "1",
            "-I",
            &link.device,
            &link.gateway,
        ],
        &[],
        Duration::from_secs(3),
    )
    .map(|r| r.success)
    .unwrap_or(false)
}

/// What the kernel's neighbour table says about the gateway.
///
/// The fallback for gateways that drop ICMP, which plenty of school and
/// corporate APs do. `STALE` is not evidence of a problem — it means the
/// entry has simply not been confirmed lately — so it maps to `Unknown`.
fn neighbour_health(link: &Uplink) -> LinkHealth {
    let Ok(result) = run_with_timeout(
        "ip",
        &["-4", "neigh", "show", &link.gateway, "dev", &link.device],
        &[],
        Duration::from_secs(3),
    ) else {
        return LinkHealth::Unknown;
    };
    parse_neighbour_health(&result.stdout)
}

fn parse_neighbour_health(neigh: &str) -> LinkHealth {
    let line = neigh.trim();
    if line.is_empty() {
        // No entry at all. On a live link there would be one, but a link
        // that has just come up has not needed the gateway yet.
        return LinkHealth::Unknown;
    }
    if line.contains("REACHABLE") || line.contains("DELAY") || line.contains("PROBE") {
        return LinkHealth::Up;
    }
    if line.contains("FAILED") || line.contains("INCOMPLETE") {
        return LinkHealth::Down;
    }
    LinkHealth::Unknown
}

/// Is the network under the tunnel still there?
pub fn health() -> LinkHealth {
    decide(
        uplink().as_ref(),
        connected_uplinks().is_empty(),
        |l| gateway_answers(l),
        neighbour_health,
    )
}

/// The decision, separated from the commands that feed it so it can be
/// tested without a network.
fn decide<P, N>(
    link: Option<&Uplink>,
    no_connected_devices: bool,
    ping: P,
    neighbour: N,
) -> LinkHealth
where
    P: Fn(&Uplink) -> bool,
    N: Fn(&Uplink) -> LinkHealth,
{
    // NetworkManager saying no device is connected is unambiguous, and
    // cheap enough to ask first.
    if no_connected_devices {
        return LinkHealth::Down;
    }
    let Some(link) = link else {
        // A connected device with no physical default route is odd but not
        // proof of anything — a captive portal state, or a route table
        // mid-rewrite during a connect.
        return LinkHealth::Unknown;
    };
    if ping(link) {
        return LinkHealth::Up;
    }
    neighbour(link)
}

// --- where traffic actually leaves ------------------------------------------

/// The address the egress question is asked about.
///
/// RFC 5737 documentation space: guaranteed never to be a real destination,
/// so nothing on a normal machine has a host route for it and the answer is
/// purely "what does the default path look like right now". Asking about a
/// live address instead — 1.1.1.1 is the tempting one — risks reading back
/// somebody's DNS host route rather than the tunnel.
const EGRESS_PROBE_ADDR: &str = "198.51.100.1";

/// Does a device name belong to a VPN tunnel this tool could have built?
///
/// Proton's tunnel surfaces as `proton0` (protun), `tunN` (OpenVPN) or a
/// `wg*` device (WireGuard), depending on protocol. `tailscale0` is a `tun`
/// device too and is deliberately excluded: it is a second tunnel that can
/// legitimately own the default route, and counting it would report "we are
/// tunnelled" about somebody else's tunnel.
fn is_tunnel_device(device: &str) -> bool {
    if device.starts_with("tailscale") {
        return false;
    }
    device.starts_with("proton") || device.starts_with("tun") || device.starts_with("wg")
}

/// Does the machine's traffic actually leave through a tunnel?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressPath {
    /// The kernel sends public traffic out of a tunnel device.
    Tunnelled(String),
    /// The kernel sends public traffic out of something else — whatever
    /// NetworkManager or Proton's status may claim, there is no tunnel
    /// carrying this machine's packets.
    Bypassed(String),
    /// The route could not be read. Never treated as either answer.
    Unknown,
}

impl EgressPath {
    /// Positive evidence that traffic is *not* tunnelled. Deliberately not
    /// `!= Tunnelled`, for the same reason [`LinkHealth::is_down`] is not
    /// `!= Up`: `Unknown` must not decide anything.
    pub fn is_bypassed(&self) -> bool {
        matches!(self, EgressPath::Bypassed(_))
    }
}

/// Ask the kernel which device public traffic leaves by.
///
/// This is a routing-table lookup, not a network round trip: it costs about
/// a millisecond and answers while the tunnel it is asked about is dead.
/// That is the whole point — every other way of telling whether a tunnel
/// works has to send a packet through it and wait out a timeout, and a
/// tunnel that never came up at all is knowable long before that.
pub fn egress_path() -> EgressPath {
    let Ok(result) = run_with_timeout(
        "ip",
        &["-4", "route", "get", EGRESS_PROBE_ADDR],
        &[],
        Duration::from_secs(3),
    ) else {
        return EgressPath::Unknown;
    };
    if !result.success {
        return EgressPath::Unknown;
    }
    parse_egress_path(&result.stdout)
}

fn parse_egress_path(route: &str) -> EgressPath {
    let Some(device) = route.split_whitespace().collect::<Vec<_>>().windows(2).find_map(|pair| {
        (pair[0] == "dev").then(|| pair[1].to_string())
    }) else {
        return EgressPath::Unknown;
    };
    if is_tunnel_device(&device) {
        EgressPath::Tunnelled(device)
    } else {
        EgressPath::Bypassed(device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_physical_default_route_is_not_a_tunnel() {
        // The exact state that made `pvpn up` report "verified in 0s" while
        // every packet was leaving unencrypted over the hotspot.
        let route = "198.51.100.1 via 172.20.10.1 dev wlp0s20f3 src 172.20.10.2 uid 1000 \n    cache ";
        assert_eq!(
            parse_egress_path(route),
            EgressPath::Bypassed("wlp0s20f3".to_string())
        );
        assert!(parse_egress_path(route).is_bypassed());
    }

    #[test]
    fn proton_and_openvpn_devices_both_count_as_tunnelled() {
        assert_eq!(
            parse_egress_path("198.51.100.1 via 10.2.0.1 dev proton0 src 10.2.0.2 uid 1000 \n    cache "),
            EgressPath::Tunnelled("proton0".to_string())
        );
        assert_eq!(
            parse_egress_path("198.51.100.1 via 10.8.0.1 dev tun0 src 10.8.0.2 uid 1000 \n    cache "),
            EgressPath::Tunnelled("tun0".to_string())
        );
    }

    #[test]
    fn tailscale_is_somebody_elses_tunnel() {
        // A `tun` device by name, but counting it would report "we are
        // tunnelled" about a tunnel this tool did not build.
        let route = "198.51.100.1 dev tailscale0 src 100.71.61.85 uid 1000 \n    cache ";
        assert!(parse_egress_path(route).is_bypassed());
    }

    #[test]
    fn an_unreadable_route_decides_nothing() {
        assert_eq!(parse_egress_path(""), EgressPath::Unknown);
        assert!(!parse_egress_path("").is_bypassed());
    }

    const WIFI_ONLY: &str = "\
wlp0s20f3:wifi:connected
tailscale0:tun:connected (externally)
docker0:bridge:connected (externally)
lo:loopback:connected (externally)
enp2s0:ethernet:unavailable
";

    fn link() -> Uplink {
        Uplink {
            device: "wlp0s20f3".into(),
            gateway: "172.20.10.1".into(),
        }
    }

    #[test]
    fn a_tunnel_device_is_never_mistaken_for_the_uplink() {
        // Verbatim shape of the table while a tunnel is up: Proton's
        // default sits at a lower metric, the real one is still below it.
        // Picking the first line would return the tunnel and then check
        // whether the dead tunnel is alive, which always says yes.
        let routes = "\
default via 10.2.0.1 dev proton0 proto static metric 50
default via 172.20.10.1 dev wlp0s20f3 proto dhcp src 172.20.10.2 metric 600
";
        let uplinks = parse_connected_uplinks(WIFI_ONLY);
        assert_eq!(
            parse_uplink(routes, &uplinks),
            Some(link()),
            "the physical link is the one that survives the tunnel"
        );
    }

    #[test]
    fn tailscale_and_docker_are_not_uplinks() {
        // Both report `connected (externally)`; neither is a way off this
        // machine, and treating tailscale0 as the uplink would ask a
        // second tunnel whether the first one's network is alive.
        assert_eq!(parse_connected_uplinks(WIFI_ONLY), vec!["wlp0s20f3"]);
    }

    #[test]
    fn no_connected_device_is_the_one_unambiguous_answer() {
        assert_eq!(
            decide(None, true, |_| false, |_| LinkHealth::Unknown),
            LinkHealth::Down
        );
    }

    #[test]
    fn a_gateway_that_answers_settles_it() {
        assert_eq!(
            decide(Some(&link()), false, |_| true, |_| LinkHealth::Down),
            LinkHealth::Up,
            "a reply outranks whatever the neighbour table thinks"
        );
    }

    #[test]
    fn a_silent_gateway_falls_back_to_the_neighbour_table() {
        assert_eq!(
            decide(Some(&link()), false, |_| false, |_| LinkHealth::Up),
            LinkHealth::Up
        );
        assert_eq!(
            decide(Some(&link()), false, |_| false, |_| LinkHealth::Down),
            LinkHealth::Down
        );
    }

    #[test]
    fn a_gateway_that_ignores_icmp_is_unknown_not_down() {
        // The case that decides whether this check helps or hurts. Plenty
        // of APs drop ICMP; calling that "the network died" would stop
        // `pvpn up` working down the list on exactly the networks it
        // exists for.
        let stale = "172.20.10.1 dev wlp0s20f3 lladdr 62:7e:c9:0d:2e:64 STALE";
        assert_eq!(parse_neighbour_health(stale), LinkHealth::Unknown);
        assert_eq!(
            decide(Some(&link()), false, |_| false, |l| neighbour_of(l, stale)),
            LinkHealth::Unknown
        );
    }

    #[test]
    fn the_kernel_giving_up_on_the_gateway_is_evidence() {
        assert_eq!(
            parse_neighbour_health("172.20.10.1 dev wlp0s20f3 FAILED"),
            LinkHealth::Down
        );
        assert_eq!(
            parse_neighbour_health("172.20.10.1 dev wlp0s20f3 INCOMPLETE"),
            LinkHealth::Down
        );
    }

    #[test]
    fn a_confirmed_neighbour_is_a_live_link() {
        assert_eq!(
            parse_neighbour_health("172.20.10.1 dev wlp0s20f3 lladdr 62:7e:c9:0d:2e:64 REACHABLE"),
            LinkHealth::Up
        );
    }

    #[test]
    fn an_empty_neighbour_table_is_not_a_verdict() {
        assert_eq!(parse_neighbour_health(""), LinkHealth::Unknown);
    }

    #[test]
    fn only_down_ever_changes_a_decision() {
        assert!(LinkHealth::Down.is_down());
        assert!(!LinkHealth::Unknown.is_down());
        assert!(!LinkHealth::Up.is_down());
    }

    fn neighbour_of(_l: &Uplink, text: &str) -> LinkHealth {
        parse_neighbour_health(text)
    }
}

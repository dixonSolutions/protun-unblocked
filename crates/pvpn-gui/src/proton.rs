//! Proton's own client settings — NetShield, kill switch, port forwarding,
//! custom DNS, VPN Accelerator, moderate NAT, IPv6, crash reports — set the
//! way Proton's CLI sets them (`protonvpn config set`), so its plan checks
//! and validation apply, and read back from `protonvpn config list` (which
//! also says what the plan allows) with `settings.json` as the fast first
//! answer.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `(label, value)` pairs, as `protonvpn config set <key> <value>` takes.
    Choice(&'static [(&'static str, &'static str)]),
    /// `on` / `off`.
    Toggle,
    /// `on --dns a,b` / `off`.
    Dns,
}

pub struct Setting {
    pub key: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
    pub kind: Kind,
}

pub const SETTINGS: [Setting; 8] = [
    Setting {
        key: "netshield",
        title: "NetShield",
        subtitle: "Blocks malware, ads and trackers at the DNS level",
        kind: Kind::Choice(&[("Off", "off"), ("Block malware", "malware-only"), ("Block malware, ads and trackers", "malware-ads-trackers")]),
    },
    Setting {
        key: "kill-switch",
        title: "Kill switch",
        subtitle: "Proton's own: blocks internet if the tunnel drops while connected. pvpn already guards its connects; on a network that kills tunnels this one also blocks you until you disconnect.",
        kind: Kind::Choice(&[("Off", "off"), ("Standard", "standard")]),
    },
    Setting {
        key: "port-forwarding",
        title: "Port forwarding",
        subtitle: "Lets P2P peers reach you through the tunnel",
        kind: Kind::Toggle,
    },
    Setting {
        key: "vpn-accelerator",
        title: "VPN Accelerator",
        subtitle: "Proton's speed optimisation on long-distance connections",
        kind: Kind::Toggle,
    },
    Setting {
        key: "moderate-nat",
        title: "Moderate NAT",
        subtitle: "Friendlier NAT for games and P2P",
        kind: Kind::Toggle,
    },
    Setting {
        key: "ipv6",
        title: "IPv6",
        subtitle: "Route IPv6 through the tunnel",
        kind: Kind::Toggle,
    },
    Setting {
        key: "custom-dns",
        title: "Custom DNS",
        subtitle: "Your own resolvers instead of Proton's (comma-separated)",
        kind: Kind::Dns,
    },
    Setting {
        key: "anonymous-crash-reports",
        title: "Anonymous crash reports",
        subtitle: "Send Proton's client crash reports without identifying you",
        kind: Kind::Toggle,
    },
];

/// What `protonvpn config list` prints for a setting the plan does not
/// include.
pub const UPGRADE: &str = "Upgrade to enable";

/// `protonvpn config list`: a two-column table after a dashed rule.
pub fn parse_config_list(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("---") {
            in_table = true;
            continue;
        }
        if !in_table || trimmed.is_empty() {
            if in_table && trimmed.is_empty() {
                break;
            }
            continue;
        }
        // Columns are separated by two or more spaces; values may contain one.
        if let Some(idx) = trimmed.find("  ") {
            let key = trimmed[..idx].trim();
            let value = trimmed[idx..].trim();
            if !key.is_empty() {
                out.insert(key.to_string(), value.to_string());
            }
        }
    }
    out
}

/// The same answers from `~/.config/Proton/VPN/settings.json` — instant,
/// but blind to the plan.
pub fn from_settings_json(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let onoff = |b: Option<bool>| b.map(|b| if b { "on" } else { "off" }.to_string());
    if let Some(n) = v.pointer("/features/netshield").and_then(|x| x.as_i64()) {
        out.insert("netshield".into(), match n { 1 => "malware-only", 2 => "malware-ads-trackers", _ => "off" }.into());
    }
    if let Some(n) = v.get("killswitch").and_then(|x| x.as_i64()) {
        out.insert("kill-switch".into(), if n == 0 { "off" } else { "standard" }.into());
    }
    for (key, ptr) in [
        ("port-forwarding", "/features/port_forwarding"),
        ("vpn-accelerator", "/features/vpn_accelerator"),
        ("moderate-nat", "/features/moderate_nat"),
        ("ipv6", "/ipv6"),
        ("anonymous-crash-reports", "/anonymous_crash_reports"),
        ("custom-dns", "/custom_dns/enabled"),
    ] {
        if let Some(s) = onoff(v.pointer(ptr).and_then(|x| x.as_bool())) {
            out.insert(key.into(), s);
        }
    }
    out
}

/// The custom DNS addresses in `settings.json`, whatever shape Proton
/// stores each entry in (a string, or an object with `ip`).
pub fn custom_dns_ips(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    v.pointer("/custom_dns/ip_list")
        .and_then(|l| l.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|e| e.as_str().map(str::to_string).or_else(|| e.get("ip").and_then(|i| i.as_str()).map(str::to_string)))
                .collect()
        })
        .unwrap_or_default()
}

/// Is this answer "on"? `config list` may say `on`, `enabled`, or a level.
pub fn is_on(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    !(v.is_empty() || v == "off" || v == "disabled" || v.starts_with("upgrade"))
}

pub fn set_command(key: &str, value: &str, dns: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = ["protonvpn", "config", "set", key, value].iter().map(|s| s.to_string()).collect();
    if let Some(list) = dns.filter(|l| !l.trim().is_empty()) {
        argv.push("--dns".into());
        argv.push(list.split([',', ' ']).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(","));
    }
    argv
}

pub fn settings_path() -> std::path::PathBuf {
    crate::settings::dirs_home().join(".config/Proton/VPN/settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\nCurrent configuration\nSetting                  Value\n\
-----------------------  -----------------\n\
netshield                Upgrade to enable\n\
kill-switch              off\n\
ipv6                     on\n\
anonymous-crash-reports  on\n\
\nTo upgrade to VPN Plus visit: https://account.protonvpn.com/pricing\n";

    #[test]
    fn config_list_parses_and_stops_at_the_table_end() {
        let m = parse_config_list(LIST);
        assert_eq!(m.get("netshield").map(String::as_str), Some(UPGRADE));
        assert_eq!(m.get("kill-switch").map(String::as_str), Some("off"));
        assert_eq!(m.get("anonymous-crash-reports").map(String::as_str), Some("on"));
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn settings_json_maps_to_the_cli_vocabulary() {
        let json = r#"{"anonymous_crash_reports":true,"custom_dns":{"enabled":false,"ip_list":[{"ip":"1.1.1.1"},"9.9.9.9"]},
            "features":{"moderate_nat":false,"netshield":2,"port_forwarding":false,"vpn_accelerator":true},
            "ipv6":true,"killswitch":0,"protocol":"protun-tls"}"#;
        let m = from_settings_json(json);
        assert_eq!(m["netshield"], "malware-ads-trackers");
        assert_eq!(m["kill-switch"], "off");
        assert_eq!(m["vpn-accelerator"], "on");
        assert_eq!(m["custom-dns"], "off");
        assert_eq!(custom_dns_ips(json), ["1.1.1.1", "9.9.9.9"]);
    }

    #[test]
    fn set_commands_are_what_protons_cli_takes() {
        assert_eq!(set_command("ipv6", "off", None), ["protonvpn", "config", "set", "ipv6", "off"]);
        assert_eq!(
            set_command("custom-dns", "on", Some("1.1.1.1, 8.8.8.8")),
            ["protonvpn", "config", "set", "custom-dns", "on", "--dns", "1.1.1.1,8.8.8.8"]
        );
        assert!(!is_on(UPGRADE));
        assert!(is_on("malware-only"));
    }
}

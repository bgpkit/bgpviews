//! Configuration: one TOML file is the whole control plane.
//!
//! Only the fields `run` currently acts on are accepted. A key this build does not
//! implement is rejected rather than ignored, because a configuration file that
//! silently drops a setting is worse than one that refuses to load: the operator
//! believes a thing is configured and the archive disagrees.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

/// One view a client exports.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewConfig {
    /// The view name, which becomes the collector suffix: `adj-in-pre`,
    /// `adj-in-post`, `loc-rib`.
    pub name: String,
    /// Interval length as a number of minutes, e.g. `15`.
    pub updates_interval_minutes: i64,
    /// Whether to write a RIB at every interval boundary.
    #[serde(default = "default_rib")]
    pub rib: bool,
}

fn default_rib() -> bool {
    true
}

/// One configured feed source.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Client name, lowercase ASCII, used in paths.
    pub name: String,
    /// Where to listen for this client's BMP session.
    pub listen: SocketAddr,
    /// Source addresses allowed to connect. The source IP is the credential, since
    /// BMP has no authentication of its own.
    pub allow_from: Vec<String>,
    /// The view this client's stream carries.
    pub view: ViewConfig,
    /// `bmp` (default) or `bgp`.
    #[serde(default)]
    pub feed: Option<String>,
    /// Our AS on a `bgp` feed. Ignored for `bmp`.
    #[serde(default)]
    pub asn: Option<u32>,
    /// Optional `max_prefixes` ceiling for a `bgp` feed.
    #[serde(default)]
    pub max_prefixes: Option<u64>,
}

/// The whole configuration file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The archive root.
    pub archive: ArchiveConfig,
    /// Feed sources.
    pub clients: Vec<ClientConfig>,
}

/// Archive-wide settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveConfig {
    /// Where the archive lives.
    pub root: PathBuf,
    /// Used percentage at which RIB writing stops. Defaults to 90.
    #[serde(default = "default_disk_warning")]
    pub disk_warning_percent: u8,
    /// Used percentage at which the session closes. Defaults to 97.
    #[serde(default = "default_disk_critical")]
    pub disk_critical_percent: u8,
    /// Days to keep a session's journal. 0 keeps every part forever; a negative value
    /// expires every part that is already closed, which exists so a test can exercise
    /// retention without waiting a month.
    #[serde(default = "default_retention_days")]
    pub journal_retention_days: i64,
}

fn default_disk_warning() -> u8 {
    90
}

fn default_disk_critical() -> u8 {
    97
}

fn default_retention_days() -> i64 {
    30
}

/// Why a configuration could not be used.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Read(std::io::Error),
    /// The TOML could not be parsed or had unknown keys.
    Parse(toml::de::Error),
    /// A value is out of range or inconsistent.
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "cannot read configuration: {e}"),
            Self::Parse(e) => write!(f, "cannot parse configuration: {e}"),
            Self::Invalid(detail) => write!(f, "invalid configuration: {detail}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Load and validate a configuration file.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when the file cannot be read or parsed, when a
    /// client or view name is not usable in a path, when an interval is not a whole
    /// number of minutes, or when a client is configured twice.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        let config: Self = toml::from_str(&text).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    /// Check the values the design constrains.
    ///
    /// # Errors
    /// Returns [`ConfigError::Invalid`] with the first problem found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.clients.is_empty() {
            return Err(ConfigError::Invalid(
                "no clients: nothing would be collected".to_string(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for client in &self.clients {
            if !is_path_component(&client.name) {
                return Err(ConfigError::Invalid(format!(
                    "client name {:?} must be lowercase ASCII letters, digits, '-' or '_'",
                    client.name
                )));
            }
            if !seen.insert(client.name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "client {:?} is configured twice",
                    client.name
                )));
            }
            if !is_path_component(&client.view.name) {
                return Err(ConfigError::Invalid(format!(
                    "view name {:?} is not usable in a path",
                    client.view.name
                )));
            }
            let minutes = client.view.updates_interval_minutes;
            if minutes < 1 {
                return Err(ConfigError::Invalid(format!(
                    "client {:?} has a zero interval",
                    client.name
                )));
            }
            if let Some(feed) = &client.feed {
                if !matches!(feed.as_str(), "bmp" | "bgp") {
                    return Err(ConfigError::Invalid(format!(
                        "client {:?} has feed {feed:?}, expected bmp or bgp",
                        client.name
                    )));
                }
                if feed == "bgp" && client.asn.is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "client {:?} uses the bgp feed and needs `asn`",
                        client.name
                    )));
                }
            }
            if client.allow_from.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "client {:?} allows no source addresses, so nobody can connect",
                    client.name
                )));
            }
            for source in &client.allow_from {
                parse_source(source).ok_or_else(|| {
                    ConfigError::Invalid(format!(
                        "client {:?} has allow_from entry {source:?}, which is not an IPv4 or IPv6 address or prefix",
                        client.name
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// The FRR configuration a router needs for this client, ready to paste.
    #[must_use]
    pub fn frr_snippet(client: &ClientConfig, listen_port: u16) -> String {
        let view = &client.view.name;
        let monitor = match view.as_str() {
            "adj-in-post" => "post-policy",
            _ => "pre-policy",
        };
        format!(
            "bmp targets {name}\n bmp connect {host} port {port} min-retry 1000 max-retry 2000\n bmp monitor ipv4 unicast {monitor}\n bmp monitor ipv6 unicast {monitor}\n",
            name = client.name,
            host = "<this-collector-address>",
            port = listen_port,
            monitor = monitor,
        )
    }
}

fn is_path_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Parse an address or prefix from an allowlist entry.
#[must_use]
pub fn parse_source(text: &str) -> Option<(std::net::IpAddr, u8)> {
    if let Ok(addr) = text.parse::<std::net::IpAddr>() {
        let bits = if addr.is_ipv4() { 32 } else { 128 };
        return Some((addr, bits));
    }
    let net = text.parse::<ipnet::IpNet>().ok()?;
    let bits = net.prefix_len();
    Some((net.addr(), bits))
}

/// Whether `peer` is inside an allowlist entry.
#[must_use]
pub fn source_allowed(peer: std::net::IpAddr, allow_from: &[String]) -> bool {
    allow_from.iter().any(|entry| match parse_source(entry) {
        Some((addr, bits)) => same_family(peer, addr) && prefix_contains(peer, addr, bits),
        None => false,
    })
}

fn same_family(a: std::net::IpAddr, b: std::net::IpAddr) -> bool {
    a.is_ipv4() == b.is_ipv4()
}

fn prefix_contains(peer: std::net::IpAddr, network: std::net::IpAddr, bits: u8) -> bool {
    match (peer, network) {
        (std::net::IpAddr::V4(p), std::net::IpAddr::V4(n)) => {
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits.min(32))
            };
            (u32::from(p) & mask) == (u32::from(n) & mask)
        }
        (std::net::IpAddr::V6(p), std::net::IpAddr::V6(n)) => {
            let mask = if bits == 0 {
                0u128
            } else {
                u128::MAX << (128 - bits.min(128))
            };
            (u128::from(p) & mask) == (u128::from(n) & mask)
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            archive: ArchiveConfig {
                root: PathBuf::from("/tmp/archive"),
                disk_warning_percent: 90,
                disk_critical_percent: 97,
                journal_retention_days: 30,
            },
            clients: vec![ClientConfig {
                name: "edge1".to_string(),
                listen: "0.0.0.0:17901".parse().unwrap(),
                allow_from: vec!["172.31.250.0/24".to_string()],
                view: ViewConfig {
                    name: "adj-in-pre".to_string(),
                    updates_interval_minutes: 15,
                    rib: true,
                },
                feed: None,
                asn: None,
                max_prefixes: None,
            }],
        }
    }

    #[test]
    fn a_minimal_configuration_parses_and_validates() {
        let text = r#"
[archive]
root = "/tmp/archive"

[[clients]]
name = "edge1"
listen = "0.0.0.0:17901"
allow_from = ["172.31.250.0/24"]

  [clients.view]
  name = "adj-in-pre"
  updates_interval_minutes = 15
"#;
        let config: Config = toml::from_str(text).expect("parses");
        config.validate().expect("valid");
        assert_eq!(config.clients[0].view.updates_interval_minutes, 15);
    }

    #[test]
    fn an_unknown_key_is_an_error_not_a_silent_default() {
        let text = r#"
[archive]
root = "/tmp/archive"
compression = "zstd"

[[clients]]
name = "edge1"
listen = "0.0.0.0:17901"
allow_from = ["172.31.250.0/24"]

  [clients.view]
  name = "adj-in-pre"
  updates_interval_minutes = 15
"#;
        assert!(
            toml::from_str::<Config>(text).is_err(),
            "unknown key rejected"
        );
    }

    #[test]
    fn a_names_that_would_escape_the_archive_are_refused() {
        let mut config = sample();
        config.clients[0].name = "../etc".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_zero_interval_is_refused() {
        let mut config = sample();
        config.clients[0].view.updates_interval_minutes = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_allowlist_matches_by_prefix_and_family() {
        let allow = vec!["172.31.250.0/24".to_string(), "2001:db8::1".to_string()];
        assert!(source_allowed("172.31.250.12".parse().unwrap(), &allow));
        assert!(!source_allowed("172.31.251.12".parse().unwrap(), &allow));
        assert!(source_allowed("2001:db8::1".parse().unwrap(), &allow));
        assert!(!source_allowed("2001:db8::2".parse().unwrap(), &allow));
        assert!(!source_allowed("10.0.0.1".parse().unwrap(), &allow));
    }

    #[test]
    fn the_frr_snippet_names_the_monitoring_mode() {
        let client = &sample().clients[0];
        let snippet = Config::frr_snippet(client, 17901);
        assert!(snippet.contains("bmp connect <this-collector-address> port 17901"));
        assert!(snippet.contains("bmp monitor ipv4 unicast pre-policy"));
    }
}

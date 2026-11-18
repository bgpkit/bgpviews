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
    /// The view name, which becomes the collector suffix: `adj-in-pre` or `adj-in-post`.
    /// `loc-rib` and `loc-rib-filtered` are refused by validation until this build routes
    /// RFC 9069 Local Route Monitoring frames.
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
    /// Our BGP identifier on a `bgp` feed, as an IPv4 address. Required there, because a
    /// speaker that originates a session must name itself before any message exists; null
    /// is the `bmp` case, where the identifier comes from the router's Peer Up.
    #[serde(default)]
    pub bgp_id: Option<String>,
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
    /// A single frame taking longer than this many milliseconds to write is a stall.
    #[serde(default = "default_write_stall_ms")]
    pub write_stall_ms: u64,
    /// Stalled milliseconds tolerated in one interval before the session closes.
    #[serde(default = "default_backpressure_deadline")]
    pub backpressure_deadline_seconds: u64,
    /// Days to keep a session's journal. 0 keeps every part forever; a negative value
    /// expires every part that is already closed, which exists so a test can exercise
    /// retention without waiting a month.
    #[serde(default = "default_retention_days")]
    pub journal_retention_days: i64,
    /// Bytes after which the open journal part is closed and the next one started. Defaults
    /// to 256 MiB; 0 disables rotation on size, which no operator should want: the active
    /// part is the one retention can never expire.
    #[serde(default = "default_journal_max_bytes")]
    pub journal_max_bytes: u64,
    /// Minutes a journal part may stay open before it is closed. Defaults to 360.
    #[serde(default = "default_journal_rotate_minutes")]
    pub journal_rotate_interval_minutes: u64,
    /// Bytes of journal frames committed per fsync. Defaults to 1 MiB; 0 fsyncs every frame,
    /// which costs about 2 ms per frame on this host.
    #[serde(default = "default_journal_sync_bytes")]
    pub journal_sync_bytes: u64,
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

fn default_journal_max_bytes() -> u64 {
    268_435_456
}

fn default_journal_rotate_minutes() -> u64 {
    360
}

fn default_journal_sync_bytes() -> u64 {
    1_048_576
}

fn default_write_stall_ms() -> u64 {
    2_000
}

fn default_backpressure_deadline() -> u64 {
    300
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
        // Two clients on one address is a daemon where the second bind fails and the client
        // never becomes reachable, which is the kind of failure `check` exists to catch.
        let mut listens: std::collections::BTreeMap<std::net::SocketAddr, String> =
            std::collections::BTreeMap::new();
        for client in &self.clients {
            if !is_path_component(&client.name) {
                return Err(ConfigError::Invalid(format!(
                    "client name {:?} must be lowercase ASCII letters, digits, '-' or '_'",
                    client.name
                )));
            }
            if let Some(other) = listens.insert(client.listen, client.name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "clients {other:?} and {:?} both listen on {}",
                    client.name, client.listen
                )));
            }
            if !seen.insert(client.name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "client {:?} is configured twice",
                    client.name
                )));
            }
            if matches!(client.view.name.as_str(), "loc-rib" | "loc-rib-filtered") {
                // Accepted and then ignored is the worst of the three outcomes: the operator
                // configures a view, the archive looks like it is collecting, and neither `run`
                // nor `status` says the view never received a record. Section 4 routes those two
                // views from RFC 9069 Local Route Monitoring frames, matched per OPEN family
                // set, which this build does not do.
                return Err(ConfigError::Invalid(format!(
                    "client {:?} asks for view {:?}, which this build does not route: an RFC 9069 \
                     Local Route Monitoring frame has no peer OPEN to match it against, and a view \
                     that is accepted but never written is worse than one that is refused. Use \
                     `adj-in-pre` or `adj-in-post`, or wait for Loc-RIB support",
                    client.name, client.view.name
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
                if feed == "bgp" && client.bgp_id.is_none() {
                    // Section 4: a speaker that originates a session has to name itself
                    // before the first message, so there is no Peer Up to take it from.
                    return Err(ConfigError::Invalid(format!(
                        "client {:?} uses the bgp feed and needs `bgp_id`, an IPv4 address",
                        client.name
                    )));
                }
                if let Some(id) = &client.bgp_id {
                    if id.parse::<std::net::Ipv4Addr>().is_err() {
                        return Err(ConfigError::Invalid(format!(
                            "client {:?} has bgp_id {id:?}, which is not an IPv4 address",
                            client.name
                        )));
                    }
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

    /// The router configuration this client's feed needs, ready to paste.
    ///
    /// A `bmp` client is monitored, so the snippet is a BMP block. A `bgp` client is peered
    /// with, so the router needs a neighbour whose remote AS is ours: printing a BMP block for
    /// it is advice that cannot work, because the router has no BMP target to point at.
    #[must_use]
    pub fn snippet(client: &ClientConfig, listen_port: u16) -> String {
        if client.feed.as_deref() == Some("bgp") {
            let asn = client.asn.unwrap_or(0);
            let id = client.bgp_id.as_deref().unwrap_or("<unset>");
            return format!(
                "# bgp feed: this collector originates the session as AS {asn}, id {id}.\n\
                 # On the router, point a neighbour at the collector's address and give it our AS:\n\
                 neighbor <this-collector-address> remote-as {asn}\n\
                 neighbor <this-collector-address> ebgp-multihop 2\n"
            );
        }
        Self::frr_snippet(client, listen_port)
    }

    /// The FRR configuration a router needs for a monitored client, ready to paste.
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
                write_stall_ms: 2_000,
                backpressure_deadline_seconds: 300,
                journal_max_bytes: 0,
                journal_rotate_interval_minutes: 0,
                journal_sync_bytes: 0,
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
                bgp_id: None,
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
    fn a_loc_rib_view_is_refused_instead_of_archiving_nothing() {
        for view in ["loc-rib", "loc-rib-filtered"] {
            let mut config = sample();
            config.clients[0].view.name = view.to_string();
            match config.validate() {
                Err(ConfigError::Invalid(message)) => {
                    assert!(message.contains(view), "{message}");
                    assert!(message.contains("RFC 9069"), "{message}");
                }
                other => panic!("expected {view} to be refused, got {other:?}"),
            }
        }
        // The two views that are routed still validate.
        for view in ["adj-in-pre", "adj-in-post"] {
            let mut config = sample();
            config.clients[0].view.name = view.to_string();
            assert!(config.validate().is_ok(), "{view} is routed");
        }
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
    fn two_clients_on_one_address_are_refused_by_name() {
        let mut config = sample();
        let mut twin = config.clients[0].clone();
        twin.name = "edge2".to_string();
        config.clients.push(twin);
        let refused = config.validate();
        match refused {
            Err(ConfigError::Invalid(message)) => {
                assert!(
                    message.contains("edge1") && message.contains("edge2"),
                    "{message}"
                );
                assert!(message.contains("17901"), "{message}");
            }
            other => panic!("expected a duplicate-listen refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_bgp_client_must_name_its_own_identifier() {
        let mut config = sample();
        config.clients[0].feed = Some("bgp".to_string());
        config.clients[0].asn = Some(64512);
        let refused = config.validate();
        assert!(
            refused.is_err(),
            "a bgp feed originates the session, so it has to name itself"
        );

        config.clients[0].bgp_id = Some("192.0.2.9".to_string());
        config
            .validate()
            .expect("a bgp client with bgp_id validates");

        config.clients[0].bgp_id = Some("not-an-address".to_string());
        assert!(
            config.validate().is_err(),
            "bgp_id has to be an IPv4 address"
        );
    }

    #[test]
    fn a_bgp_client_gets_a_neighbour_snippet_not_a_bmp_one() {
        let mut client = sample().clients[0].clone();
        client.feed = Some("bgp".to_string());
        client.asn = Some(64512);
        client.bgp_id = Some("192.0.2.9".to_string());
        let snippet = Config::snippet(&client, 17901);
        assert!(snippet.contains("remote-as 64512"), "{snippet}");
        assert!(!snippet.contains("bmp monitor"), "{snippet}");

        // A bmp client still gets the monitoring block.
        let bmp = Config::snippet(&sample().clients[0], 17901);
        assert!(bmp.contains("bmp monitor"), "{bmp}");
    }

    #[test]
    fn the_frr_snippet_names_the_monitoring_mode() {
        let client = &sample().clients[0];
        let snippet = Config::frr_snippet(client, 17901);
        assert!(snippet.contains("bmp connect <this-collector-address> port 17901"));
        assert!(snippet.contains("bmp monitor ipv4 unicast pre-policy"));
    }
}

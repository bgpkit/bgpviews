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
    /// The view name, which becomes the collector suffix: `adj-in-pre`, `adj-in-post`, `adj-out`.
    ///
    /// `adj-out` records what the router sent its peers (RFC 8671, the `O` flag); those frames
    /// travel on their own client because a collector records one view per listening port.
    /// `loc-rib` and `loc-rib-filtered` are routed from RFC 9069 Local Route Monitoring frames
    /// and are accepted on a `bmp` client with `rib = false`; a `bgp` client archives
    /// `adj-in-pre` only.
    pub name: String,
    /// Interval length as a number of minutes. Defaults to 5, the cadence RIPE RIS publishes
    /// updates at, so a new archive lines up with the public ones without being told to.
    #[serde(default = "default_updates_interval")]
    pub updates_interval_minutes: i64,
    /// Whether to write RIB snapshots at all. Off by default: one full-table RIB is about 85 MB,
    /// so at the update cadence it is gigabytes a day for a table nobody asked to snapshot, and
    /// an operator who wants snapshots asks for them.
    #[serde(default)]
    pub rib: bool,
    /// Minutes between RIB snapshots when `rib = true`. Defaults to 480, the 8-hour cadence RIPE
    /// RIS uses. Must be a whole multiple of `updates_interval_minutes`: a RIB is written when an
    /// update interval closes on a boundary that is also a multiple of this, so a value that is
    /// not a multiple would never come due.
    #[serde(default = "default_rib_interval")]
    pub rib_interval_minutes: i64,
}

fn default_updates_interval() -> i64 {
    5
}

fn default_rib_interval() -> i64 {
    480
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
    /// How a peer that announced ADD-PATH send capability is handled. `reject` refuses the
    /// session at Peer Up; `detect` accepts it and classifies every Route Monitoring message.
    /// Defaults to `reject`, which never writes a record a reader could decode differently.
    #[serde(default)]
    pub add_path: AddPathSetting,
}

/// `archive.add_path`: how a router that announced ADD-PATH send capability is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AddPathSetting {
    /// Refuse the session at Peer Up unless the router announced no ADD-PATH send capability.
    #[default]
    Reject,
    /// Accept the session and decide per Route Monitoring message whether its NLRI carries path
    /// identifiers; a message that fits both readings is skipped and named `add_path_ambiguous`.
    Detect,
}

impl From<AddPathSetting> for bgpviews_core::collector::AddPathMode {
    fn from(setting: AddPathSetting) -> Self {
        match setting {
            AddPathSetting::Reject => Self::Reject,
            AddPathSetting::Detect => Self::Detect,
        }
    }
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
            // The RIB cadence rides the update grid: a snapshot is written when an update interval
            // closes on a boundary that is also a multiple of `rib_interval_minutes`. A RIB
            // interval that is not a whole multiple of the update interval never lands on that
            // grid, so the configuration would accept a cadence that can never come due.
            if client.view.rib {
                let update = client.view.updates_interval_minutes;
                let rib = client.view.rib_interval_minutes;
                if update < 1 || rib < 1 || rib % update != 0 {
                    return Err(ConfigError::Invalid(format!(
                        "client {:?} asks for `rib = true` with updates_interval_minutes = {update} \
                         and rib_interval_minutes = {rib}; the RIB interval must be a whole \
                         multiple of the update interval, or no snapshot ever comes due",
                        client.name
                    )));
                }
            }
            if matches!(client.view.name.as_str(), "loc-rib" | "loc-rib-filtered") {
                // RFC 9069 type 3 frames only exist on a BMP feed; a BGP feed carries the
                // Adj-RIB-In of one session and cannot produce a Loc-RIB instance.
                if client.feed.as_deref() == Some("bgp") {
                    return Err(ConfigError::Invalid(format!(
                        "client {:?} asks for view {:?}, which is fed by RFC 9069 Local Route \
                         Monitoring frames and therefore needs `feed = \"bmp\"`",
                        client.name, client.view.name
                    )));
                }
                if client.view.rib {
                    // The RIB derivation assumes one global Adj-RIB-In per peer address. A
                    // Loc-RIB instance's peer address is zero-filled, so two instances would
                    // land in the same slot and one instance's routes would be archived as the
                    // other's.
                    return Err(ConfigError::Invalid(format!(
                        "client {:?} asks for view {:?} with `rib = true`; the RIB derivation is \
                         per peer address and a Loc-RIB peer has no address, so it cannot back a \
                         RIB snapshot",
                        client.name, client.view.name
                    )));
                }
            }
            // Design section 11: a `bgp` client accepts only `adj-in-pre`, and a `bmp` client
            // accepts the five routed views. Accepting a name that no frame can ever match
            // archives nothing while `status` reports a healthy empty collector.
            const BMP_VIEWS: [&str; 5] = [
                "adj-in-pre",
                "adj-in-post",
                "loc-rib",
                "loc-rib-filtered",
                "adj-out",
            ];
            let feed = client.feed.as_deref().unwrap_or("bmp");
            let known = match feed {
                "bgp" => client.view.name == "adj-in-pre",
                _ => BMP_VIEWS.contains(&client.view.name.as_str()),
            };
            if !known {
                return Err(ConfigError::Invalid(format!(
                    "client {:?} asks for view {:?}, which a `{feed}` feed cannot produce; \
                     a `bgp` client archives `adj-in-pre` only, and a `bmp` client one of {}",
                    client.name,
                    client.view.name,
                    BMP_VIEWS.join(", ")
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
        if view == "adj-out" {
            // FRR's `bmp monitor` covers Adj-RIB-In only, so printing a monitor line here would
            // tell an operator to configure something that cannot produce this view.
            return format!(
                "# {name}: view `adj-out` records what the router sends its peers (RFC 8671),\n\
                 # which FRR does not export. Point a sender that does, such as focld, at\n\
                 # <this-collector-address>:{port}\n",
                name = client.name,
                port = listen_port,
            );
        }
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

    /// A view no frame can ever match archives nothing while `status` reports a healthy empty
    /// collector, so validation refuses it: `bgp` feeds only produce `adj-in-pre`, and a `bmp`
    /// client names one of the five routed views.
    #[test]
    fn a_view_no_feed_can_produce_is_refused() {
        let mut cfg = sample();
        cfg.clients[0].feed = Some("bgp".to_string());
        cfg.clients[0].view.name = "adj-out".to_string();
        assert!(
            cfg.validate().is_err(),
            "a bgp feed cannot produce adj-out frames"
        );

        let mut cfg = sample();
        cfg.clients[0].feed = Some("bmp".to_string());
        cfg.clients[0].view.name = "bogus-view".to_string();
        assert!(cfg.validate().is_err(), "an unknown view has no routing");

        let mut cfg = sample();
        cfg.clients[0].feed = Some("bmp".to_string());
        cfg.clients[0].view.name = "adj-out".to_string();
        assert!(cfg.validate().is_ok(), "adj-out is a routed view");
    }

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
                add_path: AddPathSetting::Reject,
            },
            clients: vec![ClientConfig {
                name: "edge1".to_string(),
                listen: "0.0.0.0:17901".parse().unwrap(),
                allow_from: vec!["172.31.250.0/24".to_string()],
                view: ViewConfig {
                    name: "adj-in-pre".to_string(),
                    updates_interval_minutes: 15,
                    rib: true,
                    rib_interval_minutes: 15,
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
    fn the_defaults_follow_the_public_archives() {
        // A new archive should line up with how RIS publishes without being told: 5-minute updates,
        // and no RIB at all, because one full-table RIB is about 85 MB and nobody asked for
        // snapshots. An operator who wants them turns them on and gets RIS's 8-hour cadence.
        let text = r#"
[archive]
root = "/tmp/whatever"

[[clients]]
name = "r2"
listen = "0.0.0.0:17901"
allow_from = ["192.0.2.1/32"]

[clients.view]
name = "adj-in-pre"
"#;
        let config: Config = toml::from_str(text).expect("parses");
        config.validate().expect("valid");
        let view = &config.clients[0].view;
        assert_eq!(
            view.updates_interval_minutes, 5,
            "RIS publishes updates every 5 minutes"
        );
        assert!(!view.rib, "RIB snapshots are off unless asked for");
        assert_eq!(
            view.rib_interval_minutes, 480,
            "RIS publishes RIBs every 8 hours"
        );
    }

    #[test]
    fn a_rib_interval_that_never_lands_on_the_update_grid_is_refused() {
        // A snapshot comes due when an update interval closes on a boundary that is a multiple of
        // the RIB interval. 90 is not a multiple of 20, so no boundary would ever be both and the
        // configuration would accept a cadence that silently never fires.
        let text = r#"
[archive]
root = "/tmp/whatever"

[[clients]]
name = "r2"
listen = "0.0.0.0:17901"
allow_from = ["192.0.2.1/32"]

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 20
rib = true
rib_interval_minutes = 90
"#;
        let config: Config = toml::from_str(text).expect("parses");
        let err = config.validate().expect_err("refused");
        let text = format!("{err}");
        assert!(text.contains("whole"), "the message says why: {text}");
    }

    #[test]
    fn a_zero_interval_is_refused() {
        let mut config = sample();
        config.clients[0].view.updates_interval_minutes = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_loc_rib_view_validates_only_where_it_can_be_routed() {
        for view in ["loc-rib", "loc-rib-filtered"] {
            // A BMP feed without a RIB snapshot is exactly the shape a Loc-RIB view needs.
            let mut config = sample();
            config.clients[0].view.name = view.to_string();
            config.clients[0].view.rib = false;
            config.clients[0].feed = Some("bmp".to_string());
            assert!(
                config.validate().is_ok(),
                "{view} is routed from type 3 frames"
            );

            // A Loc-RIB peer has no address, so it cannot back a per-address RIB snapshot.
            let mut config = sample();
            config.clients[0].view.name = view.to_string();
            config.clients[0].view.rib = true;
            config.clients[0].feed = Some("bmp".to_string());
            match config.validate() {
                Err(ConfigError::Invalid(message)) => {
                    assert!(message.contains("rib = true"), "{message}");
                }
                other => panic!("expected {view} with rib = true to be refused, got {other:?}"),
            }

            // A BGP feed carries one session's Adj-RIB-In and cannot produce a Loc-RIB.
            let mut config = sample();
            config.clients[0].view.name = view.to_string();
            config.clients[0].view.rib = false;
            config.clients[0].feed = Some("bgp".to_string());
            match config.validate() {
                Err(ConfigError::Invalid(message)) => {
                    assert!(message.contains("bmp"), "{message}");
                }
                other => panic!("expected {view} on a bgp feed to be refused, got {other:?}"),
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

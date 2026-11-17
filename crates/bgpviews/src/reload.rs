//! SIGHUP configuration reload, limited to what a running session can honestly change.
//!
//! The rule this module enforces: a reload never changes the identity of the session
//! that is already open. The client name selects the archive directory, the listen
//! address and allow list select who may connect, the feed decides how bytes are read,
//! and the interval decides segment names and boundaries. Changing any of those under a
//! live session would mean the archive's own index no longer describes what happened,
//! so they are refused and reported, and take effect at the next session.
//!
//! What is left is the set of settings whose value at the moment of use is the only
//! thing that matters: whether RIB snapshots are written.
//!
//! An invalid file is never fatal. A reload that cannot be parsed leaves the running
//! configuration alone: the alternative is a daemon that stops archiving because someone
//! saved a typo.

use std::path::Path;

use bgpviews_core::collector::Collector;

use crate::config::{ClientConfig, Config};

/// What a reload did, in the order a human would read it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Settings applied to the running session.
    pub applied: Vec<String>,
    /// Settings refused, with the reason.
    pub refused: Vec<String>,
    /// Set when the file could not be read or parsed; nothing was applied.
    pub error: Option<String>,
}

/// Re-read `path` and apply what a live session may accept.
///
/// `running` is the client configuration the session was opened with, not a fresh parse:
/// a refused field must be compared against what is actually in use.
pub fn reload(path: &Path, running: &ClientConfig, collector: &mut Collector) -> Outcome {
    let mut outcome = Outcome::default();
    let fresh = match Config::load(path) {
        Ok(config) => config,
        Err(e) => {
            outcome.error = Some(e.to_string());
            return outcome;
        }
    };
    let Some(client) = fresh.clients.iter().find(|c| c.name == running.name) else {
        outcome.refused.push(format!(
            "client {} is gone from the configuration; the running session keeps archiving",
            running.name
        ));
        return outcome;
    };

    // Identity and routing of the session: refused, because the archive already records
    // a session that these values described.
    if client.listen != running.listen {
        outcome.refused.push(format!(
            "listen {} -> {} takes effect at the next session",
            running.listen, client.listen
        ));
    }
    if client.allow_from != running.allow_from {
        outcome.refused.push(format!(
            "allow_from change takes effect at the next session (was {:?}, now {:?})",
            running.allow_from, client.allow_from
        ));
    }
    if client.feed != running.feed {
        outcome
            .refused
            .push("a feed change needs a new client name and a new session".to_string());
    }
    if client.asn != running.asn || client.max_prefixes != running.max_prefixes {
        outcome
            .refused
            .push("asn/max_prefixes changes take effect at the next session".to_string());
    }
    if client.view.name != running.view.name {
        outcome.refused.push(format!(
            "view {} -> {} takes effect at the next session",
            running.view.name, client.view.name
        ));
    }
    if client.view.updates_interval_minutes != running.view.updates_interval_minutes {
        outcome.refused.push(format!(
            "interval {} -> {} minutes takes effect at the next session: the open segment's name and boundary are already fixed",
            running.view.updates_interval_minutes, client.view.updates_interval_minutes
        ));
    }

    // The one setting whose current value is all that matters.
    if client.view.rib != collector.rib_enabled() {
        collector.set_rib_enabled(client.view.rib);
        outcome.applied.push(format!("rib = {}", client.view.rib));
    }

    outcome
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use bgpviews_core::collector::CollectorConfig;

    fn write(dir: &Path, text: &str) -> std::path::PathBuf {
        let path = dir.join("bgpviews.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn config_text(rib: bool, interval: i64, listen: &str) -> String {
        format!(
            r#"[archive]
root = "archive"

[[clients]]
name = "r2"
listen = "{listen}"
allow_from = ["172.31.250.0/24"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = {interval}
rib = {rib}
"#
        )
    }

    fn running(rib: bool) -> ClientConfig {
        ClientConfig {
            name: "r2".to_string(),
            listen: "0.0.0.0:17901".parse().unwrap(),
            allow_from: vec!["172.31.250.0/24".to_string()],
            view: crate::config::ViewConfig {
                name: "adj-in-pre".to_string(),
                updates_interval_minutes: 60,
                rib,
            },
            feed: Some("bmp".to_string()),
            asn: None,
            bgp_id: None,
            max_prefixes: None,
        }
    }

    fn collector(dir: &Path, rib: bool) -> Collector {
        Collector::open_at(
            CollectorConfig {
                client: "r2".to_string(),
                view: "adj-in-pre".to_string(),
                feed: "bmp".to_string(),
                epoch: 1,
                interval_seconds: 3600,
                root: dir.join("archive"),
                rib,
                disk_warning_percent: 90,
                disk_critical_percent: 97,
                write_stall_ms: 2000,
                backpressure_deadline_seconds: 300,
            },
            1_763_166_169_000_000,
        )
        .unwrap()
    }

    #[test]
    fn turning_rib_off_applies_to_the_running_session() {
        let dir = std::env::temp_dir().join(format!("bgpviews-reload-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = write(&dir, &config_text(false, 60, "0.0.0.0:17901"));
        let mut collector = collector(&dir, true);
        assert!(collector.rib_enabled());

        let outcome = reload(&path, &running(true), &mut collector);
        assert_eq!(outcome.applied, vec!["rib = false".to_string()]);
        assert!(outcome.refused.is_empty(), "{:?}", outcome.refused);
        assert!(!collector.rib_enabled());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_changes_are_refused_and_named() {
        let dir = std::env::temp_dir().join(format!("bgpviews-reload-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Three refusals at once: a new port, a new interval, a new allow list.
        let mut text = config_text(true, 15, "0.0.0.0:17902");
        text = text.replace(
            r#"allow_from = ["172.31.250.0/24"]"#,
            r#"allow_from = ["172.31.250.0/24", "10.0.0.0/8"]"#,
        );
        let path = write(&dir, &text);
        let mut collector = collector(&dir, true);

        let outcome = reload(&path, &running(true), &mut collector);
        assert!(outcome.applied.is_empty());
        let text = outcome.refused.join("\n");
        assert!(
            text.contains("listen 0.0.0.0:17901 -> 0.0.0.0:17902"),
            "{text}"
        );
        assert!(text.contains("interval 60 -> 15"), "{text}");
        assert!(text.contains("allow_from"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_leaves_the_running_configuration_alone() {
        let dir = std::env::temp_dir().join(format!("bgpviews-reload-c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = write(&dir, "this is not toml = = =");
        let mut collector = collector(&dir, true);

        let outcome = reload(&path, &running(true), &mut collector);
        assert!(outcome.error.is_some(), "a parse failure must be reported");
        assert!(outcome.applied.is_empty());
        assert!(collector.rib_enabled(), "the running setting must survive");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

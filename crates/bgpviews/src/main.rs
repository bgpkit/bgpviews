//! `bgpviews`: one binary, one configuration file, one output directory.
//!
//! `run` serves one session per configured client and exits when that session ends.
//! That is deliberately smaller than the design's long-running daemon: it is enough
//! to prove the live path against a real router, and it does not pretend to handle
//! reconnection, epochs or signals yet. `status`, `verify` and `rebuild-index` are
//! designed and not implemented; they say so and exit 2.

mod config;

use std::io::Write;
use std::net::TcpListener;
use std::process::ExitCode;

use bgpviews_core::bgp_feed::{serve, FeedConfig};
use bgpviews_core::collector::{now_micros, pump, Collector, CollectorConfig};

use config::Config;

const USAGE: &str = "\
bgpviews: self-hosted BGP archive

Usage: bgpviews <command> [options]

Commands:
  run --config FILE            accept one BMP session per client and archive it
  check --config FILE          validate configuration and print router snippets
  status --config FILE         show coverage, session state and gaps (not implemented)
  verify --config FILE         reconcile the catalog against the disk (not implemented)
  rebuild-index --config FILE  rebuild the catalog (not implemented)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if matches!(command.as_str(), "-V" | "--version") {
        println!("bgpviews {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let path = match config_path(&args) {
        Some(path) => path,
        None => {
            eprintln!("bgpviews: {command} needs --config FILE");
            return ExitCode::from(2);
        }
    };

    match command.as_str() {
        "run" => run(&path),
        "check" => check(&path),
        "status" | "verify" | "rebuild-index" => {
            eprintln!(
                "bgpviews: `{command}` is specified in docs/design.md and not implemented yet"
            );
            ExitCode::from(2)
        }
        other => {
            eprintln!("bgpviews: unknown command `{other}`");
            print!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn config_path(args: &[String]) -> Option<std::path::PathBuf> {
    args.iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from)
}

fn check(path: &std::path::Path) -> ExitCode {
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("bgpviews: {e}");
            return ExitCode::from(2);
        }
    };
    println!("configuration ok: {}", path.display());
    println!("archive root:     {}", config.archive.root.display());
    for client in &config.clients {
        println!();
        println!(
            "client {} listens on {} for {} (interval {}m)",
            client.name, client.listen, client.view.name, client.view.updates_interval_minutes
        );
        println!("allowed sources: {}", client.allow_from.join(", "));
        print!("{}", Config::frr_snippet(client, client.listen.port()));
        println!(
            "firewall: allow tcp from the router(s) above to this host:{}",
            client.listen.port()
        );
    }
    ExitCode::SUCCESS
}

fn run(path: &std::path::Path) -> ExitCode {
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("bgpviews: {e}");
            return ExitCode::from(2);
        }
    };
    for client in &config.clients {
        if let Err(e) = run_client(&config, client) {
            eprintln!("bgpviews: client {}: {e}", client.name);
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

fn run_client(config: &Config, client: &config::ClientConfig) -> Result<(), String> {
    let listener = TcpListener::bind(client.listen)
        .map_err(|e| format!("cannot bind {}: {e}", client.listen))?;
    eprintln!(
        "bgpviews: {} listening on {}, allowed sources {:?}",
        client.name, client.listen, client.allow_from
    );

    // One session per run: the design's daemon keeps the listener open and replaces
    // the connection when the router redials, which needs epochs and is not built yet.
    let (mut stream, peer) = listener
        .accept()
        .map_err(|e| format!("accept failed: {e}"))?;
    if !config::source_allowed(peer.ip(), &client.allow_from) {
        eprintln!(
            "bgpviews: refusing {}: not in allow_from for {}",
            peer.ip(),
            client.name
        );
        return Err("source not allowed".to_string());
    }
    eprintln!("bgpviews: accepted BMP from {}", peer.ip());

    let start = now_micros();
    let collector_config = CollectorConfig {
        client: client.name.clone(),
        view: client.view.name.clone(),
        epoch: 1,
        interval_seconds: client.view.updates_interval_minutes * 60,
        root: config.archive.root.clone(),
    };
    let mut collector = Collector::open_at(collector_config, start)
        .map_err(|e| format!("cannot open collector: {e}"))?;

    let mut clock = now_micros;
    let fed = match client.feed.as_deref().unwrap_or("bmp") {
        "bgp" => {
            let feed = FeedConfig {
                local_as: client.asn.unwrap_or(0),
                local_ip: stream
                    .local_addr()
                    .map(|a| a.ip())
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                peer_ip: peer.ip(),
                max_prefixes: client.max_prefixes,
            };
            serve(stream, &feed, &mut collector, &mut clock)
                .map_err(|e| format!("bgp feed failed: {e}"))?;
            collector.frames()
        }
        _ => pump(&mut stream, &mut collector, &mut clock)
            .map_err(|e| format!("session failed: {e}"))?,
    };
    collector
        .finish(now_micros())
        .map_err(|e| format!("cannot close session: {e}"))?;

    let segments = collector.published();
    println!("client            {}", client.name);
    println!("frames            {fed}");
    println!("records           {}", collector.records());
    println!("end of rib        {}", collector.end_of_rib());
    println!("unconfigured view {}", collector.unconfigured_view());
    println!("journal           {}", collector.journal_path().display());
    println!("coverage          {}", collector.coverage_path().display());
    println!("segments          {}", segments.len());
    for segment in segments {
        println!(
            "  {}  records={} state={} reasons={:?}",
            segment.path.display(),
            segment.sidecar.records,
            segment.sidecar.state,
            segment.sidecar.partial_reasons
        );
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

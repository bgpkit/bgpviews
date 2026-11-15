//! `bgpviews`: one binary, one configuration file, one output directory.
//!
//! The command surface is fixed by `docs/design.md` section 12. Only the parts
//! that do not need a collector are wired here yet: the process answers `check`
//! by reporting the design version it was built against, and refuses the rest
//! with an explicit "not implemented" rather than pretending to work.

use std::process::ExitCode;

const USAGE: &str = "\
bgpviews: self-hosted BGP archive

Usage: bgpviews <command> [options]

Commands:
  run --config FILE            ingest, derive, publish, and append catalog parts
  check --config FILE          validate configuration and print router snippets
  status --config FILE         show coverage, session state and gaps
  verify --config FILE         reconcile the catalog against the disk
  rebuild-index --config FILE  rebuild the catalog from persisted objects
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--version" | "-V") => {
            println!("bgpviews {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(known @ ("run" | "check" | "status" | "verify" | "rebuild-index")) => {
            eprintln!("bgpviews: `{known}` is specified in docs/design.md but not implemented yet");
            ExitCode::from(2)
        }
        Some(other) => {
            eprintln!("bgpviews: unknown command `{other}`");
            print!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

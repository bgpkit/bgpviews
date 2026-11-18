//! Print every version in a `peer_state.log`, in log order.
//!
//! The log is the archive's record of what the collector knew about each `(peer, generation,
//! family)` object, and the catalog is a projection of it. Printing it through the same replay the
//! projection uses is the only way to tell a bug in the log from a bug in a hand-written decoder:
//! the framing is a magic, then per record a sequence, a kind byte, two timestamps, a length and a
//! CRC, so a decoder that is one byte off still produces plausible-looking JSON.
//!
//! ```text
//! cargo run --example peer_state_versions -- <archive>/sessions/<client>/<epoch>/peer_state.log
//! ```

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: peer_state_versions <peer_state.log>");
        std::process::exit(2);
    };
    let versions = match bgpviews_core::peer_state::replay(std::path::Path::new(&path)) {
        Ok(versions) => versions,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    println!("{} version(s)", versions.len());
    for (seq, version) in versions {
        println!(
            "seq {:>3} afi {:>2} safi {:>2} v{:>2} {:?} up {:?} down {:?} last {:?} reconstructed {}",
            seq,
            version.key.afi,
            version.key.safi,
            version.state_version,
            version.state,
            version.peer_up_received_at,
            version.peer_down_received_at,
            version.last_received_at,
            version.reconstructed
        );
    }
}

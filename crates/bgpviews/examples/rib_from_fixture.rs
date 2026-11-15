//! Derive one RIB file from a captured BMP stream and write it into an archive.
//!
//! This exists so the RIB projection can be checked without a live session: the
//! fixture is the same one the replay path uses, the frames are split the same way the
//! collector splits them, and the file lands in the RouteViews layout so `monocle` and
//! `bgpdump` can read it back. It is an example, not a command: deriving a RIB from a
//! running collector is the collector's job, and a command for it would be a second
//! way to do the same thing.
//!
//! Usage: cargo run -p bgpviews --example rib_from_fixture -- <capture.bin> <archive-root>

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use bgpviews_core::bmp::{classify_frame, FrameKind, StreamSplitter};
use bgpviews_core::rib::{frame_records, Rib, RibPeer};
use bgpviews_core::segment::{SegmentWriter, Sidecar, Stream};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let capture: PathBuf = args
        .next()
        .ok_or("usage: rib_from_fixture <capture.bin> <root>")?
        .into();
    let root: PathBuf = args
        .next()
        .ok_or("usage: rib_from_fixture <capture.bin> <root>")?
        .into();

    let bytes = std::fs::read(&capture)?;
    let mut splitter = StreamSplitter::new();
    splitter.push(&bytes);
    let mut rib = Rib::new();
    // Peer identity comes from Peer Up; a Route Monitoring frame names its peer by
    // address, so the table is filled as peers are met.
    let mut peer_info: BTreeMap<IpAddr, RibPeer> = BTreeMap::new();
    let mut updates = 0usize;
    let mut at = 0u32;

    while let Some(frame) = splitter.next_frame()? {
        match classify_frame(&frame)? {
            FrameKind::PeerUp {
                peer_ip, peer_asn, ..
            } => {
                peer_info.insert(
                    peer_ip,
                    RibPeer {
                        ip: peer_ip,
                        asn: peer_asn,
                        bgp_id: "0.0.0.0".parse()?,
                    },
                );
            }
            FrameKind::RouteMonitoring {
                peer_ip,
                peer_asn,
                pdu_offset,
                ..
            } => {
                let update = &frame[pdu_offset..];
                // The fixture carries no wall clock in the frame; the capture time is
                // the archive's, and the RIB records it once at the cutoff.
                at = 1_763_166_169;
                let peer = peer_info.get(&peer_ip).copied().unwrap_or(RibPeer {
                    ip: peer_ip,
                    asn: peer_asn,
                    bgp_id: "0.0.0.0".parse()?,
                });
                match rib.apply(peer, update, at) {
                    Ok(_) => updates += 1,
                    Err(_) => rib.note_refused(),
                }
            }
            _ => {}
        }
    }

    println!(
        "applied {updates} update(s), refused {}, {} prefix(es), {} peer(s)",
        rib.refused(),
        rib.prefixes(),
        rib.peers()
    );

    let records = rib.encode(at, "adj-in-pre");
    let bytes = frame_records(at, &records);
    let collector = "r2-adj-in-pre";
    let path = PathBuf::from(format!(
        "{collector}/bgpdata/2026.11/RIBS/ribs.20261115.0022.gz"
    ));
    let full = root.join(&path);
    std::fs::create_dir_all(full.parent().ok_or("no parent")?)?;
    let mut writer = SegmentWriter::create(&root, collector, Stream::Ribs, &path)?;
    writer.append(&bytes, i64::from(at))?;
    let sidecar = Sidecar {
        schema_version: 1,
        collector: collector.to_string(),
        client: "r2".to_string(),
        view: "adj-in-pre".to_string(),
        feed: "bmp".to_string(),
        stream: Stream::Ribs,
        range_start: bgpviews_core::segment::render_stamp(i64::from(at) * 1_000_000),
        range_end: bgpviews_core::segment::render_stamp(i64::from(at) * 1_000_000),
        session_epoch: 1,
        ordinal: 1,
        path: path.to_string_lossy().to_string(),
        bytes: 0,
        records: 0,
        end_of_rib_records: 0,
        compression: "gzip".to_string(),
        sha256: String::new(),
        state: String::new(),
        partial_reasons: Vec::new(),
        late_arrival: 0,
        exporter_sys_name: None,
        exporter_sys_descr: None,
        first_router_time: None,
        last_router_time: None,
        baseline: Vec::new(),
    };
    let published = writer.finish(sidecar, "complete", Vec::new(), 0)?;
    println!(
        "wrote {} ({} bytes, {} records)",
        published.path,
        published.bytes,
        records.len()
    );
    Ok(())
}

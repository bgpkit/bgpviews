//! Measure what a RIB-sized index costs: write time, file size, open time.
//!
//! Run with `cargo run --release --example rib_index_bench -- 100000`.

use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::Instant;

use bgpviews_core::rib_index::{write_index, Index};
use ipnet::IpNet;

fn main() {
    let count: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);

    let mut entries: Vec<(IpNet, u64)> = Vec::with_capacity(count as usize);
    for i in 0..count {
        // Spread /24s across the v4 space and every 16th entry into v6.
        if i % 16 == 15 {
            let seg = (i / 16) as u16;
            let text = format!("2001:{seg:x}::/32");
            if let Ok(prefix) = IpNet::from_str(&text) {
                entries.push((prefix, u64::from(i % 64)));
            }
        } else {
            let a = 1 + (i >> 16) as u8;
            let b = (i >> 8) as u8;
            let c = (i & 0xFF) as u8;
            let text = format!("{a}.{b}.{c}.0/24");
            if let Ok(prefix) = IpNet::from_str(&text) {
                entries.push((prefix, u64::from(i % 64)));
            }
        }
        let _ = Ipv4Addr::LOCALHOST;
    }

    let path = std::env::temp_dir().join(format!("bgpviews-bench-{count}.snap"));
    let refs: Vec<(IpNet, &u64)> = entries.iter().map(|(p, v)| (*p, v)).collect();

    let t0 = Instant::now();
    let bytes = write_index(&path, refs, "2026-11-14T12:00:00Z").unwrap_or(0);
    let write_ms = t0.elapsed().as_millis();

    let t1 = Instant::now();
    let index = Index::open(&path).unwrap_or_else(|_| unreachable_index());
    let open_ms = t1.elapsed().as_millis();

    // Look up a prefix that exists and one that does not.
    let t2 = Instant::now();
    let mut hits = 0usize;
    let mut probes = 0usize;
    for i in (0..count).step_by((count / 1000).max(1) as usize) {
        probes += 1;
        let (p, _) = &entries[i as usize];
        if let IpNet::V4(v4) = p {
            if index
                .lookup(std::net::IpAddr::V4(v4.addr()))
                .ok()
                .flatten()
                .is_some()
            {
                hits += 1;
            }
        }
    }
    let lookup_ms = t2.elapsed().as_millis();

    println!("entries      {count}");
    println!("file bytes   {bytes}");
    println!("write ms     {write_ms}");
    println!("open ms      {open_ms}");
    println!("lookup ms    {lookup_ms} for {probes} probes ({hits} hits)");
    println!("content id   {}", index.content_id());
    std::fs::remove_file(&path).ok();
}

fn unreachable_index() -> Index {
    // The benchmark is not a test; failing loudly here is intentional.
    std::process::exit(1)
}

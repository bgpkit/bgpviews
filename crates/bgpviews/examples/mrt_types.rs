//! Print the MRT type and subtype of every record in a file, with counts.
//!
//! The subtype carries contracts a reader cannot recover from the payload: an RFC 8050
//! ADD-PATH record is the same bytes as a plain one apart from the path identifier inside each
//! NLRI, so a wrong subtype is unreadable rather than merely incomplete. This prints what the
//! file actually claims, using the same parser a consumer would.
//!
//! ```text
//! cargo run --example mrt_types -- <file.mrt.gz>
//! ```

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: mrt_types <file.mrt.gz>");
        std::process::exit(2);
    };
    let parser = match bgpkit_parser::BgpkitParser::new(&path) {
        Ok(parser) => parser,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    let mut counts: std::collections::BTreeMap<(u16, u16), u64> = std::collections::BTreeMap::new();
    for record in parser.into_record_iter() {
        *counts
            .entry((
                record.common_header.entry_type.into(),
                record.common_header.entry_subtype,
            ))
            .or_default() += 1;
    }
    for ((mrt_type, subtype), count) in counts {
        println!("type {mrt_type} subtype {subtype}: {count} record(s)");
    }
}

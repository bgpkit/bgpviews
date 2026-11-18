//! Print the distinct prefixes in an MRT file, one per line.
//!
//! The point is to read an archive the way a consumer would, and to catch a class of bug a
//! subtype check cannot: a record whose subtype claims path identifiers but whose NLRI was
//! decoded without them still parses, it just yields prefixes nobody announced.
//!
//! ```text
//! cargo run --example prefixes -- <file.mrt.gz>
//! ```

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: prefixes <file.mrt.gz>");
        std::process::exit(2);
    };
    let parser = match bgpkit_parser::BgpkitParser::new(&path) {
        Ok(parser) => parser,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    let mut prefixes: Vec<String> = parser
        .into_elem_iter()
        .map(|elem| elem.prefix.prefix.to_string())
        .collect();
    prefixes.sort();
    prefixes.dedup();
    for prefix in prefixes {
        println!("{prefix}");
    }
}

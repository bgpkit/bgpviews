//! The RIB index for one `(collector, view)` at one cutoff, stored with
//! `bgpkit-snap`.
//!
//! This is the first place bgpviews depends on the snap container, and it is a
//! deliberate choice of scope: the trie is written once at a cutoff and read
//! back by a reader that only wants to look prefixes up, which is exactly the
//! access pattern the container is for. Nothing here mutates a published index;
//! a new cutoff writes a new object.
//!
//! The stored value is the number of peer generations contributing a path for
//! that prefix at the cutoff. That is the smallest value that is still a real
//! one: it answers "how many of my peers announced this" without needing the
//! full path set, and it keeps the container contract (`SnapValue`) trivial
//! while the collector that produces the real value is still being written.

use std::net::IpAddr;
use std::path::Path;

use bgpkit_snap::{Opts, Snapshot, Writer};
use ipnet::IpNet;

/// Why an index could not be written or read.
#[derive(Debug)]
pub enum IndexError {
    /// The container rejected the write.
    Snap(bgpkit_snap::Error),
    /// The path could not be represented as UTF-8, which the container requires
    /// in its current API.
    NonUtf8Path(String),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snap(e) => write!(f, "snap: {e}"),
            Self::NonUtf8Path(p) => write!(f, "path is not valid UTF-8: {p}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<bgpkit_snap::Error> for IndexError {
    fn from(value: bgpkit_snap::Error) -> Self {
        Self::Snap(value)
    }
}

/// Build an index from `(prefix, contributing peers)` pairs and write it to
/// `path`, returning the number of bytes written.
///
/// # Errors
/// Returns [`IndexError`] when the container rejects the value type or the write,
/// or when the path is not UTF-8.
pub fn write_index<'a, I>(path: &Path, entries: I, created_at: &str) -> Result<u64, IndexError>
where
    I: IntoIterator<Item = (IpNet, &'a u64)>,
{
    let mut writer: Writer<u64> = Writer::new();
    for (prefix, peers) in entries {
        writer.insert(prefix, *peers);
    }
    let bytes = writer.finish(&Opts {
        created_at,
        stream: None,
        window: None,
        extras: &[],
    })?;
    let text = path
        .to_str()
        .ok_or_else(|| IndexError::NonUtf8Path(path.display().to_string()))?;
    std::fs::write(text, &bytes).map_err(|e| IndexError::Snap(bgpkit_snap::Error::Io(e)))?;
    Ok(bytes.len() as u64)
}

/// One lookup result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The prefix that matched, which is not necessarily the one queried.
    pub prefix: IpNet,
    /// The stored value.
    pub peers: u64,
}

/// A read-only index that keeps the container's mapping alive for the life of
/// the value.
pub struct Index {
    snapshot: Snapshot<u64>,
}

impl Index {
    /// Open an index at `path`.
    ///
    /// # Errors
    /// Returns [`IndexError`] when the file cannot be opened, fails its own
    /// checks, or was written for a different value type.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let text = path
            .to_str()
            .ok_or_else(|| IndexError::NonUtf8Path(path.display().to_string()))?;
        let snapshot = Snapshot::<u64>::open(text).map_err(IndexError::Snap)?;
        Ok(Self { snapshot })
    }

    /// Longest-prefix match for an address.
    ///
    /// # Errors
    /// Returns [`IndexError`] if the archived value cannot be read back into a
    /// plain `u64`. In practice this is the type check the container already
    /// performed at open time, so a caller may treat it as impossible.
    pub fn lookup(&self, addr: IpAddr) -> Result<Option<Hit>, IndexError> {
        let Some((prefix, archived)) = self.snapshot.search(addr) else {
            return Ok(None);
        };
        let peers = rkyv::deserialize::<u64, rkyv::rancor::Error>(archived)
            .map_err(|_| IndexError::Snap(bgpkit_snap::Error::BadValue))?;
        Ok(Some(Hit { prefix, peers }))
    }

    /// Every entry, for a caller that wants to walk the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshot.len()
    }

    /// Whether the index holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.snapshot.is_empty()
    }

    /// The container's own content identifier, which is what a sidecar should
    /// record so a reader can prove it opened the object the collector wrote.
    #[must_use]
    pub fn content_id(&self) -> String {
        self.snapshot.content_id()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn fixtures() -> Vec<(IpNet, u64)> {
        vec![
            (IpNet::from_str("10.0.0.0/8").unwrap(), 3),
            (IpNet::from_str("10.1.0.0/16").unwrap(), 5),
            (IpNet::from_str("192.0.2.0/24").unwrap(), 2),
            (IpNet::from_str("2001:db8::/32").unwrap(), 7),
        ]
    }

    #[test]
    fn writes_reads_and_looks_up_longest_match() {
        let dir = std::env::temp_dir().join("bgpviews-rib-index-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rib-1.snap");
        let entries = fixtures();
        let bytes = write_index(
            &path,
            entries.iter().map(|(p, v)| (*p, v)),
            "2026-11-14T12:00:00Z",
        )
        .unwrap();
        assert!(bytes > 0, "the writer produced no bytes");

        let index = Index::open(&path).unwrap();
        assert_eq!(index.len(), entries.len());
        assert!(!index.is_empty());

        let hit = index
            .lookup(IpAddr::from_str("10.1.2.3").unwrap())
            .unwrap()
            .expect("10.1.2.3 is inside 10.0.0.0/8");
        assert_eq!(hit.prefix, IpNet::from_str("10.1.0.0/16").unwrap());
        assert_eq!(hit.peers, 5);

        let fallback = index
            .lookup(IpAddr::from_str("10.9.9.9").unwrap())
            .unwrap()
            .expect("10.9.9.9 is inside 10.0.0.0/8");
        assert_eq!(fallback.prefix, IpNet::from_str("10.0.0.0/8").unwrap());
        assert_eq!(fallback.peers, 3);

        assert!(index
            .lookup(IpAddr::from_str("203.0.113.1").unwrap())
            .unwrap()
            .is_none());
        assert_eq!(
            index
                .lookup(IpAddr::from_str("2001:db8::1").unwrap())
                .unwrap()
                .map(|h| h.peers),
            Some(7)
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn content_id_is_stable_for_the_same_content() {
        let dir = std::env::temp_dir().join("bgpviews-rib-index-test");
        std::fs::create_dir_all(&dir).unwrap();
        let entries = fixtures();
        let a = dir.join("rib-a.snap");
        let b = dir.join("rib-b.snap");
        write_index(
            &a,
            entries.iter().map(|(p, v)| (*p, v)),
            "2026-11-14T12:00:00Z",
        )
        .unwrap();
        // Same content, written in a different order: the container is expected
        // to be order independent.
        let mut reversed = entries.clone();
        reversed.reverse();
        write_index(
            &b,
            reversed.iter().map(|(p, v)| (*p, v)),
            "2026-11-14T12:00:00Z",
        )
        .unwrap();
        let ida = Index::open(&a).unwrap().content_id();
        let idb = Index::open(&b).unwrap().content_id();
        assert_eq!(ida, idb, "content id depends on insertion order");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }
}

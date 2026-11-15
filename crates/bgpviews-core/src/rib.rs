//! The Adj-RIB-In projection: TABLE_DUMP_V2 written from what the collector saw.
//!
//! Design reference: `docs/design.md` section 6 ("RIB") and section 7 ("MRT output").
//!
//! Two decisions are visible in this file.
//!
//! **Attributes are copied, not rebuilt.** The path attributes of a route are the
//! bytes the peer sent, walked only far enough to strip the NLRI out of MP_REACH
//! (a RIB entry carries its prefix out of band). Rebuilding them from a parsed
//! element would silently drop every attribute this crate does not model, which for
//! an archive is the difference between a record and a summary.
//!
//! **The trie is a map here, not a snap.** `bgpkit-snap` is the on-disk index for
//! lookups by prefix; the RIB a collector keeps in memory only needs to answer "what
//! do I have for this prefix from this peer", which a `BTreeMap` does in a few
//! compares. The snap index is measured separately (`docs/experiments.md`, E10).
//!
//! focl's encoder was not reused: its `SnapshotRoute::prefix` is an `Ipv4Addr`, so a
//! v6 route cannot be represented at all (recorded in `docs/features.md`).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;

/// MRT type for TABLE_DUMP_V2.
const TABLE_DUMP_V2: u16 = 13;
const PEER_INDEX_TABLE: u16 = 1;
const RIB_IPV4_UNICAST: u16 = 2;
const RIB_IPV6_UNICAST: u16 = 4;

/// Peer type bits in the peer index table.
const PEER_IS_IPV6: u8 = 0x01;
const PEER_IS_AS4: u8 = 0x02;

/// What the RIB needs to know about a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RibPeer {
    /// The peer's address.
    pub ip: IpAddr,
    /// The peer's AS number, as a 32-bit value.
    pub asn: u32,
    /// The peer's BGP identifier, when the session carried one.
    pub bgp_id: Ipv4Addr,
}

/// One route, as it will be written.
#[derive(Debug, Clone)]
struct RibRoute {
    /// When this path was first seen, in seconds.
    originated: u32,
    /// The path attributes, with any NLRI removed from MP_REACH.
    attributes: Vec<u8>,
}

/// An Adj-RIB-In, keyed by prefix and then by peer.
#[derive(Debug, Default)]
pub struct Rib {
    peers: Vec<RibPeer>,
    peer_index: BTreeMap<IpAddr, u16>,
    routes: BTreeMap<IpNet, BTreeMap<u16, RibRoute>>,
    /// Updates applied, for the sidecar.
    applied: u64,
    /// Prefixes refused because the message could not be understood.
    refused: u64,
}

/// Why an update could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RibError {
    /// The message is not a well-formed BGP UPDATE.
    Malformed(&'static str),
    /// The prefix length is impossible for its address family.
    BadPrefixLen(u8),
    /// More peers than a RIB entry can index.
    TooManyPeers,
}

impl std::fmt::Display for RibError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed UPDATE: {why}"),
            Self::BadPrefixLen(len) => write!(f, "prefix length {len} does not fit its family"),
            Self::TooManyPeers => {
                write!(f, "the peer index table cannot hold more than 65535 peers")
            }
        }
    }
}

impl std::error::Error for RibError {}

impl Rib {
    /// An empty RIB.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one UPDATE sent by `peer`.
    ///
    /// Returns the number of announcements and withdrawals applied. A message that
    /// cannot be read is refused whole: nothing is applied from a partial parse.
    ///
    /// # Errors
    /// Returns [`RibError`] for a message that is not a well-formed UPDATE.
    pub fn apply(
        &mut self,
        peer: RibPeer,
        update: &[u8],
        at: u32,
    ) -> Result<(usize, usize), RibError> {
        // Parse before registering the peer: a refused update must leave the RIB
        // exactly as it was, including the peer table.
        let body = update_body(update)?;
        let raw_attributes = attributes_original(update)?;
        let peer_index = self.peer_slot(peer)?;

        let mut cursor = 0usize;
        let withdrawn_len = usize::from(read_u16(body, &mut cursor)?);
        let withdrawn = take(body, &mut cursor, withdrawn_len)?;
        let attr_len = usize::from(read_u16(body, &mut cursor)?);
        let _ = take(body, &mut cursor, attr_len)?;
        let announced = &body[cursor..];

        // One attribute buffer serves both families: the prefix lives in the entry
        // header, so MP_REACH keeps its next hop and loses only its NLRI.
        let attributes = strip_mp_reach_nlri(raw_attributes);

        let mut applications = 0usize;
        let mut removals = 0usize;
        for prefix in read_classic_nlri(withdrawn, 4)? {
            if self.withdraw(&prefix, peer_index) {
                removals += 1;
            }
        }
        for prefix in read_classic_nlri(announced, 4)? {
            self.insert(&prefix, peer_index, &attributes, at);
            applications += 1;
        }
        if let Some((family_len, nlri)) = mp_reach_nlri(raw_attributes) {
            for prefix in read_classic_nlri(nlri, family_len)? {
                self.insert(&prefix, peer_index, &attributes, at);
                applications += 1;
            }
        }
        if let Some((family_len, nlri)) = mp_unreach_nlri(raw_attributes) {
            for prefix in read_classic_nlri(nlri, family_len)? {
                if self.withdraw(&prefix, peer_index) {
                    removals += 1;
                }
            }
        }

        self.applied += 1;
        Ok((applications, removals))
    }

    fn insert(&mut self, prefix: &IpNet, peer_index: u16, attributes: &[u8], at: u32) {
        let route = RibRoute {
            originated: at,
            attributes: attributes.to_vec(),
        };
        let entry = self.routes.entry(*prefix).or_default();
        // First sighting wins for the timestamp: a re-announcement of the same path
        // does not reset when the path arrived.
        entry.entry(peer_index).or_insert(route);
    }

    fn withdraw(&mut self, prefix: &IpNet, peer_index: u16) -> bool {
        let Some(peers) = self.routes.get_mut(prefix) else {
            return false;
        };
        let removed = peers.remove(&peer_index).is_some();
        if peers.is_empty() {
            self.routes.remove(prefix);
        }
        removed
    }

    /// Remove every route from `peer`.
    pub fn remove_peer(&mut self, peer: &RibPeer) {
        let Some(index) = self.peer_index.get(&peer.ip) else {
            return;
        };
        let index = *index;
        for peers in self.routes.values_mut() {
            peers.remove(&index);
        }
        self.routes.retain(|_, peers| !peers.is_empty());
    }

    /// Prefixes held.
    #[must_use]
    pub fn prefixes(&self) -> usize {
        self.routes.len()
    }

    /// Peers indexed.
    #[must_use]
    pub fn peers(&self) -> usize {
        self.peers.len()
    }

    /// Updates applied.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Prefixes that could not be applied.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// Record a refused update.
    pub fn note_refused(&mut self) {
        self.refused += 1;
    }

    /// Write the whole RIB as a sequence of MRT records, one per record.
    ///
    /// The caller frames each record with the common header; this function returns
    /// payloads in a deterministic order so the same RIB yields the same bytes.
    #[must_use]
    pub fn encode(&self, timestamp: u32, view: &str) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::with_capacity(self.routes.len() + 2);
        out.push((PEER_INDEX_TABLE, self.encode_peer_index(view)));
        let mut sequence = 0u32;
        for (prefix, peers) in &self.routes {
            let subtype = match prefix {
                IpNet::V4(_) => RIB_IPV4_UNICAST,
                IpNet::V6(_) => RIB_IPV6_UNICAST,
            };
            out.push((subtype, encode_rib_entry(prefix, peers, &mut sequence)));
        }
        let _ = timestamp;
        out
    }

    fn peer_slot(&mut self, peer: RibPeer) -> Result<u16, RibError> {
        if let Some(index) = self.peer_index.get(&peer.ip) {
            return Ok(*index);
        }
        let index = u16::try_from(self.peers.len()).map_err(|_| RibError::TooManyPeers)?;
        self.peers.push(peer);
        self.peer_index.insert(peer.ip, index);
        Ok(index)
    }

    fn encode_peer_index(&self, view: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[10, 0, 0, 1]); // collector BGP id
        out.extend_from_slice(&u16::try_from(view.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(&view.as_bytes()[..view.len().min(u16::MAX as usize)]);
        out.extend_from_slice(&u16::try_from(self.peers.len()).unwrap_or(0).to_be_bytes());
        for peer in &self.peers {
            let ipv6 = peer.ip.is_ipv6();
            out.push(PEER_IS_AS4 | if ipv6 { PEER_IS_IPV6 } else { 0 });
            out.extend_from_slice(&peer.bgp_id.octets());
            match peer.ip {
                IpAddr::V4(v4) => out.extend_from_slice(&v4.octets()),
                IpAddr::V6(v6) => out.extend_from_slice(&v6.octets()),
            }
            out.extend_from_slice(&peer.asn.to_be_bytes());
        }
        out
    }
}

/// Frame one payload into a single MRT record.
#[must_use]
pub fn frame_records_into(timestamp: u32, subtype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&TABLE_DUMP_V2.to_be_bytes());
    out.extend_from_slice(&subtype.to_be_bytes());
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(payload);
    out
}

fn encode_rib_entry(
    prefix: &IpNet,
    peers: &BTreeMap<u16, RibRoute>,
    sequence: &mut u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    *sequence += 1;
    out.extend_from_slice(&sequence.to_be_bytes());
    out.push(prefix.prefix_len());
    let (bytes, family_len) = match prefix {
        IpNet::V4(v4) => (v4.addr().octets().to_vec(), 4),
        IpNet::V6(v6) => (v6.addr().octets().to_vec(), 16),
    };
    let keep = (usize::from(prefix.prefix_len()))
        .div_ceil(8)
        .min(family_len);
    out.extend_from_slice(&bytes[..keep]);
    out.extend_from_slice(&u16::try_from(peers.len()).unwrap_or(0).to_be_bytes());
    for (peer_index, route) in peers {
        out.extend_from_slice(&peer_index.to_be_bytes());
        out.extend_from_slice(&route.originated.to_be_bytes());
        let len = u16::try_from(route.attributes.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&route.attributes[..usize::from(len)]);
    }
    out
}

/// Frame payloads into MRT records: timestamp, type, subtype, length, payload.
///
/// The length counts everything after the common header, which is what every reader
/// in the ecosystem expects (`docs/experiments.md`, E7).
#[must_use]
pub fn frame_records(timestamp: u32, records: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (subtype, payload) in records {
        out.extend_from_slice(&timestamp.to_be_bytes());
        out.extend_from_slice(&TABLE_DUMP_V2.to_be_bytes());
        out.extend_from_slice(&subtype.to_be_bytes());
        out.extend_from_slice(
            &u32::try_from(payload.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        out.extend_from_slice(payload);
    }
    out
}

fn update_body(update: &[u8]) -> Result<&[u8], RibError> {
    if update.len() < 19 {
        return Err(RibError::Malformed("shorter than a BGP header"));
    }
    if update[..16].iter().any(|b| *b != 0xff) {
        return Err(RibError::Malformed("marker is not all ones"));
    }
    let len = usize::from(u16::from_be_bytes([update[16], update[17]]));
    if len != update.len() {
        return Err(RibError::Malformed(
            "length field disagrees with the buffer",
        ));
    }
    match update[18] {
        2 => Ok(&update[19..]),
        _ => Err(RibError::Malformed("not an UPDATE")),
    }
}

fn attributes_original(update: &[u8]) -> Result<&[u8], RibError> {
    let body = update_body(update)?;
    let mut cursor = 0usize;
    let withdrawn_len = read_u16(body, &mut cursor)? as usize;
    let _ = take(body, &mut cursor, withdrawn_len)?;
    let attr_len = read_u16(body, &mut cursor)? as usize;
    take(body, &mut cursor, attr_len)
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, RibError> {
    if *cursor + 2 > bytes.len() {
        return Err(RibError::Malformed("truncated length field"));
    }
    let value = u16::from_be_bytes([bytes[*cursor], bytes[*cursor + 1]]);
    *cursor += 2;
    Ok(value)
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], RibError> {
    if *cursor + len > bytes.len() {
        return Err(RibError::Malformed("truncated section"));
    }
    let slice = &bytes[*cursor..*cursor + len];
    *cursor += len;
    Ok(slice)
}

/// Walk a classic NLRI list (no ADD-PATH) and return the prefixes it names.
fn read_classic_nlri(bytes: &[u8], family_len: usize) -> Result<Vec<IpNet>, RibError> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let bits = bytes[cursor];
        cursor += 1;
        let octets = usize::from(bits).div_ceil(8);
        let limit = family_len * 8;
        if usize::from(bits) > limit {
            return Err(RibError::BadPrefixLen(bits));
        }
        let raw = take(bytes, &mut cursor, octets)?;
        out.push(if family_len == 4 {
            let mut addr = [0u8; 4];
            addr[..octets].copy_from_slice(raw);
            IpNet::V4(
                ipnet::Ipv4Net::new(Ipv4Addr::from(addr), bits)
                    .map_err(|_| RibError::BadPrefixLen(bits))?,
            )
        } else {
            let mut addr = [0u8; 16];
            addr[..octets].copy_from_slice(raw);
            IpNet::V6(
                ipnet::Ipv6Net::new(Ipv6Addr::from(addr), bits)
                    .map_err(|_| RibError::BadPrefixLen(bits))?,
            )
        });
    }
    Ok(out)
}

/// Walk the attributes once, handing each `(kind, flags, value)` to `visit`.
fn walk_attributes<'a>(attributes: &'a [u8], mut visit: impl FnMut(u8, u8, &'a [u8])) {
    let mut cursor = 0usize;
    while cursor + 3 <= attributes.len() {
        let flags = attributes[cursor];
        let kind = attributes[cursor + 1];
        cursor += 2;
        let len = if flags & 0x10 != 0 {
            if cursor + 2 > attributes.len() {
                return;
            }
            let value = usize::from(u16::from_be_bytes([
                attributes[cursor],
                attributes[cursor + 1],
            ]));
            cursor += 2;
            value
        } else {
            let value = usize::from(attributes[cursor]);
            cursor += 1;
            value
        };
        if cursor + len > attributes.len() {
            return;
        }
        visit(kind, flags, &attributes[cursor..cursor + len]);
        cursor += len;
    }
}

/// The AFI's address size and the NLRI inside an MP_REACH_NLRI attribute.
///
/// The NLRI here is what a RIB projection needs: the prefixes this message announced,
/// with the next hop held in the attribute that carries them.
fn mp_reach_nlri(attributes: &[u8]) -> Option<(usize, &[u8])> {
    let mut found = None;
    walk_attributes(attributes, |kind, _flags, value| {
        if kind != 14 || value.len() < 5 {
            return;
        }
        let afi = u16::from_be_bytes([value[0], value[1]]);
        let next_hop_len = usize::from(value[3]);
        let keep = 4 + next_hop_len + 1;
        if value.len() < keep {
            return;
        }
        let family_len = match afi {
            1 => 4,
            2 => 16,
            _ => return,
        };
        found = Some((family_len, &value[keep..]));
    });
    found
}

/// The AFI's address size and the NLRI inside an MP_UNREACH_NLRI attribute.
fn mp_unreach_nlri(attributes: &[u8]) -> Option<(usize, &[u8])> {
    let mut found = None;
    walk_attributes(attributes, |kind, _flags, value| {
        if kind != 15 || value.len() < 3 {
            return;
        }
        let afi = u16::from_be_bytes([value[0], value[1]]);
        let family_len = match afi {
            1 => 4,
            2 => 16,
            _ => return,
        };
        found = Some((family_len, &value[3..]));
    });
    found
}

/// Rebuild the attribute list with MP_REACH's NLRI removed and its length corrected.
///
/// A RIB entry carries its prefix in the entry header, so an MP_REACH that still named
/// the prefixes of the original UPDATE would claim routes the entry does not hold.
fn strip_mp_reach_nlri(attributes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(attributes.len());
    let mut cursor = 0usize;
    while cursor + 3 <= attributes.len() {
        let flags = attributes[cursor];
        let kind = attributes[cursor + 1];
        cursor += 2;
        let (len, len_bytes) = if flags & 0x10 != 0 {
            let value = usize::from(u16::from_be_bytes([
                attributes[cursor],
                attributes[cursor + 1],
            ]));
            (value, 2usize)
        } else {
            (usize::from(attributes[cursor]), 1usize)
        };
        if cursor + len_bytes > attributes.len() {
            break;
        }
        cursor += len_bytes;
        if cursor + len > attributes.len() {
            break;
        }
        let value = &attributes[cursor..cursor + len];
        cursor += len;

        if kind != 14 || value.len() < 5 {
            out.push(flags);
            out.push(kind);
            if len_bytes == 2 {
                out.extend_from_slice(&u16::try_from(len).unwrap_or(u16::MAX).to_be_bytes());
            } else {
                out.push(u8::try_from(len).unwrap_or(u8::MAX));
            }
            out.extend_from_slice(value);
            continue;
        }

        let next_hop_len = usize::from(value[3]);
        let keep = 4 + next_hop_len + 1;
        if value.len() < keep {
            out.push(flags);
            out.push(kind);
            out.push(u8::try_from(len).unwrap_or(u8::MAX));
            out.extend_from_slice(value);
            continue;
        }
        let trimmed = &value[..keep];
        out.push(flags);
        out.push(kind);
        out.push(u8::try_from(trimmed.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(trimmed);
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn update(withdrawn: &[u8], attributes: &[u8], nlri: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&u16::try_from(withdrawn.len()).unwrap().to_be_bytes());
        body.extend_from_slice(withdrawn);
        body.extend_from_slice(&u16::try_from(attributes.len()).unwrap().to_be_bytes());
        body.extend_from_slice(attributes);
        body.extend_from_slice(nlri);
        let mut message = vec![0xff; 16];
        let len = u16::try_from(19 + body.len()).unwrap();
        message.extend_from_slice(&len.to_be_bytes());
        message.push(2);
        message.extend_from_slice(&body);
        message
    }

    fn ipv4_attributes() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x40, 1, 1, 0]); // ORIGIN igp
        bytes.extend_from_slice(&[0x40, 2, 6, 2, 1, 0, 0, 0xfd, 0xe9]); // AS_PATH 65001
        bytes.extend_from_slice(&[0x40, 3, 4, 192, 0, 2, 1]); // NEXT_HOP
        bytes.extend_from_slice(&[0x80, 4, 4, 0, 0, 0, 50]); // MED
        bytes.extend_from_slice(&[0xc0, 8, 4, 0, 1, 0, 2]); // COMMUNITY 1:2
        bytes
    }

    fn peer() -> RibPeer {
        RibPeer {
            ip: "172.31.250.11".parse().unwrap(),
            asn: 65002,
            bgp_id: "10.0.0.2".parse().unwrap(),
        }
    }

    /// Read the encoded RIB back with bgpkit-parser's file parser, which is what a
    /// consumer actually runs. A claim about bytes that only our own decoder accepts
    /// is not a claim about MRT.
    fn read_back(bytes: &[u8]) -> Vec<bgpkit_parser::models::BgpElem> {
        let path = std::env::temp_dir().join(format!(
            "bgpviews-rib-{}-{}.mrt",
            std::process::id(),
            bytes.len()
        ));
        std::fs::write(&path, bytes).unwrap();
        let parser = bgpkit_parser::BgpkitParser::new(path.to_str().unwrap()).unwrap();
        let elems: Vec<_> = parser.into_elem_iter().collect();
        let _ = std::fs::remove_file(&path);
        elems
    }

    #[test]
    fn an_ipv4_route_reads_back_with_every_attribute_it_was_sent() {
        let mut rib = Rib::new();
        let nlri = [24u8, 192, 0, 2];
        let (announced, withdrawn) = rib
            .apply(
                peer(),
                &update(&[], &ipv4_attributes(), &nlri),
                1_763_166_169,
            )
            .unwrap();
        assert_eq!((announced, withdrawn), (1, 0));
        assert_eq!(rib.prefixes(), 1);

        let records = rib.encode(1_763_166_169, "adj-in-pre");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].0, RIB_IPV4_UNICAST);
        let bytes = frame_records(1_763_166_169, &records);

        let elems = read_back(&bytes);
        assert_eq!(elems.len(), 1, "one RIB entry is one element");
        let elem = &elems[0];
        assert_eq!(elem.prefix.prefix.to_string(), "192.0.2.0/24");
        assert_eq!(
            elem.next_hop.map(|hop| hop.to_string()),
            Some("192.0.2.1".to_string())
        );
        assert_eq!(
            elem.as_path.as_ref().map(|p| p.to_string()),
            Some("65001".to_string())
        );
        assert_eq!(format!("{:?}", elem.origin), "Some(IGP)");
        assert_eq!(elem.med, Some(50));
        // The community survives because the attributes are copied, not rebuilt.
        assert!(!elem.communities.as_deref().unwrap_or(&[]).is_empty());
    }

    #[test]
    fn an_ipv6_route_reads_back_with_its_next_hop() {
        let mut rib = Rib::new();
        let next_hop: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let mut mp_reach = Vec::new();
        mp_reach.extend_from_slice(&2u16.to_be_bytes()); // AFI IPv6
        mp_reach.push(1); // SAFI unicast
        mp_reach.push(16); // next hop length
        mp_reach.extend_from_slice(&next_hop.octets());
        mp_reach.push(0); // reserved
        mp_reach.extend_from_slice(&[32u8, 0x20, 0x01, 0x0d, 0xb8]); // 2001:db8::/32
        let mut attributes = Vec::new();
        attributes.extend_from_slice(&[0x40, 1, 1, 0]);
        attributes.extend_from_slice(&[0x40, 2, 6, 2, 1, 0, 0, 0xfd, 0xe9]);
        attributes.extend_from_slice(&[0x80, 14, u8::try_from(mp_reach.len()).unwrap()]);
        attributes.extend_from_slice(&mp_reach);

        let (announced, _) = rib
            .apply(peer(), &update(&[], &attributes, &[]), 10)
            .unwrap();
        assert_eq!(announced, 1);
        let records = rib.encode(10, "adj-in-pre");
        assert_eq!(records[1].0, RIB_IPV6_UNICAST);
        let elems = read_back(&frame_records(10, &records));

        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].prefix.prefix.to_string(), "2001:db8::/32");
        assert_eq!(
            elems[0].next_hop.map(|hop| hop.to_string()),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(
            elems[0].as_path.as_ref().map(|p| p.to_string()),
            Some("65001".to_string())
        );
    }

    #[test]
    fn a_withdrawal_removes_only_the_peer_that_sent_it() {
        let mut rib = Rib::new();
        let other = RibPeer {
            ip: "172.31.250.12".parse().unwrap(),
            asn: 65003,
            bgp_id: "10.0.0.3".parse().unwrap(),
        };
        let nlri = [24u8, 192, 0, 2];
        rib.apply(peer(), &update(&[], &ipv4_attributes(), &nlri), 1)
            .unwrap();
        rib.apply(other, &update(&[], &ipv4_attributes(), &nlri), 2)
            .unwrap();
        assert_eq!(rib.prefixes(), 1);

        let (_, withdrawn) = rib
            .apply(peer(), &update(&nlri, &ipv4_attributes(), &[]), 3)
            .unwrap();
        assert_eq!(withdrawn, 1);
        assert_eq!(rib.prefixes(), 1, "the other peer still holds it");

        let elems = read_back(&frame_records(4, &rib.encode(4, "adj-in-pre")));
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].peer_ip.to_string(), "172.31.250.12");
    }

    #[test]
    fn a_re_announcement_keeps_the_first_sighting_time() {
        let mut rib = Rib::new();
        let nlri = [24u8, 192, 0, 2];
        rib.apply(peer(), &update(&[], &ipv4_attributes(), &nlri), 100)
            .unwrap();
        rib.apply(peer(), &update(&[], &ipv4_attributes(), &nlri), 200)
            .unwrap();
        assert_eq!(rib.prefixes(), 1);
        // bgpkit-parser fills an element's timestamp from the record header, not from
        // the entry's originated_time, so the entry bytes are what prove this: seq(4),
        // prefix_len(1), prefix(3), entry_count(2), then peer(2) and originated(4).
        let records = rib.encode(200, "adj-in-pre");
        let payload = &records[1].1;
        let originated = u32::from_be_bytes([payload[12], payload[13], payload[14], payload[15]]);
        assert_eq!(originated, 100, "first sighting, not the refresh");
    }

    #[test]
    fn a_malformed_update_is_refused_and_changes_nothing() {
        let mut rib = Rib::new();
        assert!(rib.apply(peer(), &[0u8; 20], 1).is_err());
        assert_eq!(rib.prefixes(), 0);
        assert_eq!(rib.peers(), 0);
        assert_eq!(rib.applied(), 0);
    }
}

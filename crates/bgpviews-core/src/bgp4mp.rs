//! The MRT envelope: BGP4MP_ET around a BGP PDU that is copied, not re-encoded.
//!
//! Design reference: `docs/design.md` section 10. The rule that matters is that
//! the PDU bytes are the ones the router exported. Everything else in this module
//! is the eleven bytes of context a reader needs to interpret them: who the peer
//! was, who we were, which family the addresses belong to, and when.
//!
//! `focl` has an encoder for the same record shape, and it is deliberately not
//! used: it parses the PDU into `BgpMessage` and re-serializes it, which drops
//! any attribute the version of `bgpkit-parser` in use does not model. An archive
//! that silently loses unknown attributes is worse than one that carries bytes a
//! reader has to skip.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use bgpkit_parser::models::AsnLength;

/// MRT type for BGP4MP with the extended timestamp.
pub const MRT_TYPE_BGP4MP_ET: u16 = 17;
/// BGP4MP subtype for a message whose AS_PATH is two-octet encoded.
pub const BGP4MP_MESSAGE: u16 = 1;
/// BGP4MP subtype for a message whose AS_PATH is four-octet encoded.
pub const BGP4MP_MESSAGE_AS4: u16 = 4;
/// BGP4MP subtype for a two-octet message whose NLRI carries a path identifier (RFC 8050).
pub const BGP4MP_MESSAGE_ADDPATH: u16 = 8;
/// BGP4MP subtype for a four-octet message whose NLRI carries a path identifier (RFC 8050).
pub const BGP4MP_MESSAGE_AS4_ADDPATH: u16 = 9;

/// The fields the envelope needs, all of which come from the BMP headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordContext {
    /// Exporter's clock, seconds since the epoch.
    pub seconds: u32,
    /// Exporter's clock, microseconds, carried in the ET field.
    pub micros: u32,
    /// The monitored peer's AS.
    pub peer_asn: u32,
    /// Our AS on that session.
    pub local_asn: u32,
    /// The monitored peer's address.
    pub peer_ip: IpAddr,
    /// Our address on that session.
    pub local_ip: IpAddr,
    /// Interface index, zero when the exporter did not say.
    pub interface_index: u16,
    /// Width of the AS numbers inside the PDU, which selects the subtype.
    pub asn_len: AsnLength,
    /// Whether every NLRI in the PDU carries a path identifier, which selects the ADDPATH
    /// subtype. A PDU that mixes both forms has no honest subtype and is not encoded at all.
    pub add_path: bool,
}

/// Why a record could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// The peer and local addresses are not the same family, so no single AFI
    /// describes both endpoints.
    AddressFamilyMismatch {
        /// The peer address.
        peer: IpAddr,
        /// Our address.
        local: IpAddr,
    },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddressFamilyMismatch { peer, local } => write!(
                f,
                "peer {peer} and local {local} are different address families"
            ),
        }
    }
}

impl std::error::Error for EncodeError {}

/// The subtype that describes a PDU's AS width and whether its NLRI carries path identifiers.
///
/// RFC 8050 adds a subtype per AS width rather than a flag inside the record, so a reader that
/// assumes the plain subtype will read the first four octets of every NLRI as a prefix length.
#[must_use]
pub fn subtype_for(asn_len: AsnLength, add_path: bool) -> u16 {
    match (asn_len, add_path) {
        (AsnLength::Bits16, false) => BGP4MP_MESSAGE,
        (AsnLength::Bits32, false) => BGP4MP_MESSAGE_AS4,
        (AsnLength::Bits16, true) => BGP4MP_MESSAGE_ADDPATH,
        (AsnLength::Bits32, true) => BGP4MP_MESSAGE_AS4_ADDPATH,
    }
}

/// Encode one BGP4MP_ET record around `pdu`.
///
/// Layout, from RFC 6396 section 4.4.2 plus the ET extension:
/// `seconds | type | subtype | length | micros | peer_as | local_as | ifindex |
/// afi | peer_ip | local_ip | pdu`.
///
/// # Errors
/// Returns [`EncodeError`] when the two endpoint addresses are different
/// families, because the AFI field describes both and there is no truthful value.
pub fn encode_record(context: &RecordContext, pdu: &[u8]) -> Result<Vec<u8>, EncodeError> {
    if context.peer_ip.is_ipv4() != context.local_ip.is_ipv4() {
        return Err(EncodeError::AddressFamilyMismatch {
            peer: context.peer_ip,
            local: context.local_ip,
        });
    }
    let afi: u16 = if context.peer_ip.is_ipv4() { 1 } else { 2 };
    let (peer_bytes, local_bytes) = address_bytes(context.peer_ip, context.local_ip);

    let mut body = Vec::with_capacity(16 + peer_bytes.len() + local_bytes.len() + pdu.len());
    match context.asn_len {
        AsnLength::Bits16 => {
            // A 16-bit session still names its own AS in two octets, and a peer AS
            // above the 16-bit range cannot be written truthfully here.
            body.extend_from_slice(&(context.peer_asn as u16).to_be_bytes());
            body.extend_from_slice(&(context.local_asn as u16).to_be_bytes());
        }
        AsnLength::Bits32 => {
            body.extend_from_slice(&context.peer_asn.to_be_bytes());
            body.extend_from_slice(&context.local_asn.to_be_bytes());
        }
    }
    body.extend_from_slice(&context.interface_index.to_be_bytes());
    body.extend_from_slice(&afi.to_be_bytes());
    body.extend_from_slice(&peer_bytes);
    body.extend_from_slice(&local_bytes);
    body.extend_from_slice(pdu);

    let subtype = subtype_for(context.asn_len, context.add_path);
    // The length field counts everything after the 12-byte common header, which is
    // the convention every reader in this ecosystem implements: bgpkit-parser
    // states it, focl writes it, and bgpdump and monocle both stop reading a record
    // early when it is written the other way. Measured, not assumed: see the test
    // below and docs/design.md section 19.
    let body_len = 4 + body.len();
    let mut record = Vec::with_capacity(12 + body_len);
    record.extend_from_slice(&context.seconds.to_be_bytes());
    record.extend_from_slice(&MRT_TYPE_BGP4MP_ET.to_be_bytes());
    record.extend_from_slice(&subtype.to_be_bytes());
    record.extend_from_slice(&(body_len as u32).to_be_bytes());
    record.extend_from_slice(&context.micros.to_be_bytes());
    record.extend_from_slice(&body);
    Ok(record)
}

fn address_bytes(peer: IpAddr, local: IpAddr) -> (Vec<u8>, Vec<u8>) {
    match (peer, local) {
        (IpAddr::V4(p), IpAddr::V4(l)) => (v4(p), v4(l)),
        (IpAddr::V6(p), IpAddr::V6(l)) => (v6(p), v6(l)),
        // Unreachable: the caller rejects mixed families before this point.
        (IpAddr::V4(p), IpAddr::V6(_)) => (v4(p), Vec::new()),
        (IpAddr::V6(p), IpAddr::V4(_)) => (v6(p), Vec::new()),
    }
}

fn v4(addr: Ipv4Addr) -> Vec<u8> {
    addr.octets().to_vec()
}

fn v6(addr: Ipv6Addr) -> Vec<u8> {
    addr.octets().to_vec()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn context(asn_len: AsnLength) -> RecordContext {
        RecordContext {
            seconds: 1_800_000_000,
            micros: 123_456,
            peer_asn: 65001,
            local_asn: 65002,
            peer_ip: IpAddr::from_str("172.31.250.11").unwrap(),
            local_ip: IpAddr::from_str("172.31.250.12").unwrap(),
            interface_index: 0,
            asn_len,
            add_path: false,
        }
    }

    #[test]
    fn the_length_field_counts_everything_after_the_common_header() {
        let pdu = vec![0xFFu8; 19];
        let record = encode_record(&context(AsnLength::Bits32), &pdu).unwrap();
        // MRT header: seconds u32 | type u16 | subtype u16 | length u32.
        let claimed = u32::from_be_bytes([record[8], record[9], record[10], record[11]]) as usize;
        assert_eq!(
            claimed + 12,
            record.len(),
            "the length field excludes the 12-byte common header"
        );
    }

    #[test]
    fn the_as4_envelope_is_the_expected_size() {
        // 12 header + 4 micros + 4 peer_as + 4 local_as + 2 ifindex + 2 afi
        // + 4 peer ip + 4 local ip + 19 pdu
        let pdu = vec![0xFFu8; 19];
        let record = encode_record(&context(AsnLength::Bits32), &pdu).unwrap();
        assert_eq!(record.len(), 55);
        assert_eq!(&record[4..6], &MRT_TYPE_BGP4MP_ET.to_be_bytes());
        assert_eq!(&record[6..8], &BGP4MP_MESSAGE_AS4.to_be_bytes());
    }

    #[test]
    fn a_16bit_session_uses_the_short_subtype_and_two_octet_as_fields() {
        let pdu = vec![0xFFu8; 19];
        let record = encode_record(&context(AsnLength::Bits16), &pdu).unwrap();
        assert_eq!(&record[6..8], &BGP4MP_MESSAGE.to_be_bytes());
        // 12 + 4 + 2 + 2 + 2 + 2 + 4 + 4 + 19
        assert_eq!(record.len(), 51);
    }

    #[test]
    fn mixed_families_are_refused_rather_than_guessed() {
        let mut ctx = context(AsnLength::Bits32);
        ctx.local_ip = IpAddr::from_str("2001:db8::1").unwrap();
        let err = encode_record(&ctx, &[]).unwrap_err();
        assert!(matches!(err, EncodeError::AddressFamilyMismatch { .. }));
    }

    #[test]
    fn subtype_follows_the_pdu_width_not_a_preference() {
        assert_eq!(subtype_for(AsnLength::Bits16, false), BGP4MP_MESSAGE);
        assert_eq!(subtype_for(AsnLength::Bits32, false), BGP4MP_MESSAGE_AS4);
        // RFC 8050 gives the path-identifier forms their own subtypes; a reader that assumes the
        // plain one reads the first four octets of every NLRI as a prefix length.
        assert_eq!(subtype_for(AsnLength::Bits16, true), 8);
        assert_eq!(subtype_for(AsnLength::Bits32, true), 9);
    }
}

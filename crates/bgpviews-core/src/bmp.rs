//! Splitting a BMP stream into frames, and reading the two headers that decide
//! where a frame goes.
//!
//! The splitter works on the 6-byte common header only, so it can never be
//! confused by a message body, and it keeps every frame's bytes exactly as they
//! arrived. Those bytes are what the session journal stores: the archive's raw
//! record is the frame, not a parsed message, so a parser bug cannot lose data
//! that the journal already holds.
//!
//! Metadata for routing decisions comes from `bgpkit-parser`, which is the same
//! reader the rest of BGPKIT uses. The split and the parse are deliberately
//! separate: the split decides what a frame is, the parse decides where it goes.

use std::net::IpAddr;

use bgpkit_parser::bmp::messages::BmpMessageBody;
use bgpkit_parser::models::AsnLength;
use bgpkit_parser::parse_bmp_msg;
use bytes::Bytes;

/// Where a frame sits and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSpan {
    /// Byte offset of the frame in the stream.
    pub offset: usize,
    /// Length including the 6-byte common header.
    pub len: usize,
    /// BMP message type, from the common header.
    pub msg_type: u8,
    /// BMP version, which must be 3 for v1.
    pub version: u8,
}

/// A frame worth acting on, with the metadata the design needs to place it.
///
/// Not `Eq`: the exporter's clock is a float, and pretending otherwise would mean
/// rounding it before it reaches the archive.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The span this frame occupies in the stream.
    pub span: FrameSpan,
    /// The raw bytes, exactly as received.
    pub bytes: Vec<u8>,
    /// What the frame means for routing.
    pub kind: FrameKind,
}

/// The routing meaning of a frame, which is what the collector acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameKind {
    /// The exporter identified itself. Carries `sysDescr` and `sysName`.
    Initiation {
        /// `sysDescr`, if the TLV was present.
        sys_descr: Option<String>,
        /// `sysName`, if the TLV was present.
        sys_name: Option<String>,
    },
    /// A monitored peer came up. This is where local AS and local address come
    /// from, and it is the only place they come from.
    PeerUp {
        /// The monitored peer's address.
        peer_ip: IpAddr,
        /// The monitored peer's AS.
        peer_asn: u32,
        /// The local address of the monitored session.
        local_ip: IpAddr,
        /// The local AS of the monitored session, from the sent OPEN.
        local_asn: u32,
        /// The peer's declared families, as `(afi, safi)` pairs.
        families: Vec<(u16, u16)>,
    },
    /// A route was announced or withdrawn for the monitored peer.
    RouteMonitoring {
        /// The monitored peer's address.
        peer_ip: IpAddr,
        /// The monitored peer's AS.
        peer_asn: u32,
        /// Whether this record is post-policy.
        post_policy: bool,
        /// Whether this is an Adj-RIB-Out record.
        adj_rib_out: bool,
        /// Whether the PDU's NLRI and AS_PATH use 4-byte AS numbers.
        asn_len: AsnLength,
        /// Whether the PDU is an end-of-RIB marker.
        is_eor: bool,
        /// For an end-of-RIB marker, the family it closes: from MP_UNREACH when
        /// present, otherwise IPv4 unicast, which is what a classic empty UPDATE
        /// means. `None` for a marker that names no family.
        eor_family: Option<(u16, u16)>,
        /// The exporter's clock for this message, in seconds since the epoch, or
        /// `None` when the exporter sent zero, which RFC 7854 says means unknown.
        router_time: Option<f64>,
        /// Offset of the BGP PDU inside the frame, for the raw copy.
        pdu_offset: usize,
    },
    /// A monitored peer went down.
    PeerDown {
        /// The monitored peer's address.
        peer_ip: IpAddr,
        /// The monitored peer's AS.
        peer_asn: u32,
        /// The BMP reason code.
        reason: u8,
    },
    /// Anything else: statistics, mirroring, termination. Journaled, not routed.
    Other {
        /// The BMP message type.
        msg_type: u8,
    },
}

/// Why a stream could not be split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitError {
    /// The stream ended inside a common header.
    TruncatedHeader {
        /// Offset where the partial header began.
        offset: usize,
    },
    /// The stream ended inside a message body.
    TruncatedBody {
        /// Offset of the frame start.
        offset: usize,
        /// Length the header claimed.
        claimed: usize,
        /// Bytes actually available.
        available: usize,
    },
    /// The version byte was not 3.
    UnsupportedVersion {
        /// Offset of the frame start.
        offset: usize,
        /// The version found.
        version: u8,
    },
    /// The header claimed a length smaller than the header itself.
    BadLength {
        /// Offset of the frame start.
        offset: usize,
        /// The length claimed.
        claimed: usize,
    },
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TruncatedHeader { offset } => {
                write!(
                    f,
                    "stream ends inside a BMP common header at offset {offset}"
                )
            }
            Self::TruncatedBody {
                offset,
                claimed,
                available,
            } => write!(
                f,
                "frame at {offset} claims {claimed} bytes but only {available} are available"
            ),
            Self::UnsupportedVersion { offset, version } => {
                write!(f, "frame at {offset} has BMP version {version}, expected 3")
            }
            Self::BadLength { offset, claimed } => {
                write!(
                    f,
                    "frame at {offset} claims length {claimed}, below the 6-byte header"
                )
            }
        }
    }
}

impl std::error::Error for SplitError {}

/// Split a byte stream into frame spans, without parsing any body.
///
/// # Errors
/// Returns [`SplitError`] on a truncated header or body, a version other than 3,
/// or a length that cannot include its own header.
pub fn split(stream: &[u8]) -> Result<Vec<FrameSpan>, SplitError> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    while offset < stream.len() {
        if stream.len() - offset < 6 {
            return Err(SplitError::TruncatedHeader { offset });
        }
        let version = stream[offset];
        if version != 3 {
            return Err(SplitError::UnsupportedVersion { offset, version });
        }
        let claimed = u32::from_be_bytes([
            stream[offset + 1],
            stream[offset + 2],
            stream[offset + 3],
            stream[offset + 4],
        ]) as usize;
        if claimed < 6 {
            return Err(SplitError::BadLength { offset, claimed });
        }
        if stream.len() - offset < claimed {
            return Err(SplitError::TruncatedBody {
                offset,
                claimed,
                available: stream.len() - offset,
            });
        }
        spans.push(FrameSpan {
            offset,
            len: claimed,
            msg_type: stream[offset + 5],
            version,
        });
        offset += claimed;
    }
    Ok(spans)
}

/// Split and classify a stream.
///
/// The bytes of each frame are copied out, because the journal keeps them and the
/// caller must not be able to mutate the stream underneath a frame it holds.
///
/// # Errors
/// Returns [`SplitError`] from [`split`], or [`FrameError`] when a frame that
/// matters for routing cannot be read.
pub fn frames(stream: &[u8]) -> Result<Vec<Frame>, FrameError> {
    let spans = split(stream)?;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let bytes = stream[span.offset..span.offset + span.len].to_vec();
        let kind = classify(&bytes)?;
        out.push(Frame { span, bytes, kind });
    }
    Ok(out)
}

/// Why a frame could not be classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The split failed.
    Split(SplitError),
    /// The message body could not be parsed.
    Parse {
        /// Offset of the frame.
        offset: usize,
        /// The parser's message.
        detail: String,
    },
    /// A per-peer message arrived with no per-peer header.
    MissingPerPeerHeader {
        /// Offset of the frame.
        offset: usize,
    },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Split(e) => write!(f, "{e}"),
            Self::Parse { offset, detail } => {
                write!(f, "frame at {offset} could not be parsed: {detail}")
            }
            Self::MissingPerPeerHeader { offset } => {
                write!(f, "frame at {offset} carries no per-peer header")
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl From<SplitError> for FrameError {
    fn from(value: SplitError) -> Self {
        Self::Split(value)
    }
}

/// Splits a byte stream that arrives in chunks.
///
/// The live collector cannot wait for a whole capture, and it must not reimplement
/// the length arithmetic that [`split`] already does. This type keeps a buffer and
/// hands out complete frames, in order, leaving a partial frame for the next chunk.
#[derive(Debug, Default)]
pub struct StreamSplitter {
    buffer: Vec<u8>,
}

impl StreamSplitter {
    /// A splitter with no buffered bytes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes as they arrive.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// How many bytes are buffered but not yet a complete frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Take the next complete frame, if one is buffered.
    ///
    /// # Errors
    /// Returns [`SplitError`] for a version other than 3 or a length that cannot
    /// include its own header. A partial frame is not an error: it returns `None`
    /// and waits.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, SplitError> {
        if self.buffer.len() < 6 {
            return Ok(None);
        }
        let version = self.buffer[0];
        if version != 3 {
            return Err(SplitError::UnsupportedVersion { offset: 0, version });
        }
        let claimed = u32::from_be_bytes([
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
            self.buffer[4],
        ]) as usize;
        if claimed < 6 {
            return Err(SplitError::BadLength { offset: 0, claimed });
        }
        if self.buffer.len() < claimed {
            return Ok(None);
        }
        let frame: Vec<u8> = self.buffer.drain(..claimed).collect();
        Ok(Some(frame))
    }
}

/// The family an end-of-RIB marker closes.
fn eor_family_of(message: &bgpkit_parser::models::BgpMessage) -> Option<(u16, u16)> {
    use bgpkit_parser::models::{AttributeValue, BgpMessage};
    let BgpMessage::Update(update) = message else {
        return None;
    };
    for attribute in &update.attributes {
        if let AttributeValue::MpUnreachNlri(nlri) = attribute {
            return Some((u16::from(nlri.afi), u16::from(u8::from(nlri.safi))));
        }
    }
    // A classic EoR carries no attribute that names a family, and RFC 4271 gives
    // an unattributed marker to IPv4 unicast.
    Some((1, 1))
}

/// The per-peer header is 42 bytes; every per-peer message starts its body after
/// it, and a Route Monitoring frame's BGP PDU starts there.
pub const PER_PEER_HEADER_LEN: usize = 42;

/// Classify one complete frame, exactly as [`frames`] does.
///
/// Exposed for the live collector, which receives frames from a socket instead of
/// from a buffer and must classify each one the same way a replay does.
///
/// # Errors
/// Returns [`FrameError`] when the message body cannot be parsed.
pub fn classify_frame(bytes: &[u8]) -> Result<FrameKind, FrameError> {
    classify(bytes)
}

fn classify(bytes: &[u8]) -> Result<FrameKind, FrameError> {
    let offset = 0usize;
    let msg_type = bytes.get(5).copied().unwrap_or(0);
    let mut data = Bytes::copy_from_slice(bytes);
    let parsed = parse_bmp_msg(&mut data).map_err(|e| FrameError::Parse {
        offset,
        detail: e.to_string(),
    })?;

    let per_peer = parsed.per_peer_header.as_ref();
    match &parsed.message_body {
        BmpMessageBody::InitiationMessage(init) => {
            use bgpkit_parser::bmp::messages::InitiationTlvType;
            let pick = |wanted: InitiationTlvType| {
                init.tlvs
                    .iter()
                    .find(|t| t.info_type == wanted)
                    .map(|t| t.info.clone())
            };
            Ok(FrameKind::Initiation {
                sys_descr: pick(InitiationTlvType::SysDescr),
                sys_name: pick(InitiationTlvType::SysName),
            })
        }
        BmpMessageBody::PeerUpNotification(up) => {
            let header = per_peer.ok_or(FrameError::MissingPerPeerHeader { offset })?;
            let mut families = Vec::new();
            use bgpkit_parser::models::{BgpMessage, CapabilityValue, ParamValue};
            for message in [&up.sent_open, &up.received_open] {
                if let BgpMessage::Open(open) = message {
                    for param in &open.opt_params {
                        if let ParamValue::Capacities(caps) = &param.param_value {
                            for cap in caps {
                                if let CapabilityValue::MultiprotocolExtensions(mp) = &cap.value {
                                    let pair = (u16::from(mp.afi), u16::from(u8::from(mp.safi)));
                                    if !families.contains(&pair) {
                                        families.push(pair);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let local_asn = match &up.sent_open {
                bgpkit_parser::models::BgpMessage::Open(open) => u32::from(open.asn),
                _ => 0,
            };
            Ok(FrameKind::PeerUp {
                peer_ip: header.peer_ip,
                peer_asn: u32::from(header.peer_asn),
                local_ip: up.local_addr,
                local_asn,
                families,
            })
        }
        BmpMessageBody::RouteMonitoring(mon) => {
            let header = per_peer.ok_or(FrameError::MissingPerPeerHeader { offset })?;
            let is_eor = mon.is_end_of_rib();
            // An EoR's family is what MP_UNREACH names; a classic empty UPDATE is
            // IPv4 unicast, which is the family RFC 4271 gives an unattributed EoR.
            let eor_family = if is_eor {
                eor_family_of(&mon.bgp_message)
            } else {
                None
            };
            use bgpkit_parser::bmp::messages::PerPeerFlags;
            let (post_policy, adj_rib_out, asn_len) = match &header.peer_flags {
                PerPeerFlags::PeerFlags(flags) => (
                    flags.bits() & 0b0100_0000 != 0,
                    flags.bits() & 0b0001_0000 != 0,
                    flags.asn_length(),
                ),
                // RFC 9069: a Loc-RIB instance has no L, O or A bit and is always
                // four-octet encoded. It is out of scope for v1, but reading it as
                // post-policy would be a lie, so it stays pre-policy with 4-byte AS.
                PerPeerFlags::LocalRibPeerFlags(_) => (false, false, AsnLength::Bits32),
            };
            Ok(FrameKind::RouteMonitoring {
                peer_ip: header.peer_ip,
                peer_asn: u32::from(header.peer_asn),
                post_policy,
                adj_rib_out,
                asn_len,
                is_eor,
                eor_family,
                router_time: if header.timestamp > 0.0 {
                    Some(header.timestamp)
                } else {
                    None
                },
                pdu_offset: 6 + PER_PEER_HEADER_LEN,
            })
        }
        BmpMessageBody::PeerDownNotification(down) => {
            let header = per_peer.ok_or(FrameError::MissingPerPeerHeader { offset })?;
            Ok(FrameKind::PeerDown {
                peer_ip: header.peer_ip,
                peer_asn: u32::from(header.peer_asn),
                reason: u8::from(down.reason),
            })
        }
        _ => Ok(FrameKind::Other { msg_type }),
    }
}

/// Whether a peer type is one v1 archives. Types 1 and 2 (RD and Local
/// instances) and Loc-RIB are out of scope for this milestone.
#[must_use]
pub fn is_supported_peer_type(peer_type: bgpkit_parser::bmp::messages::BmpPeerType) -> bool {
    matches!(peer_type, bgpkit_parser::bmp::messages::BmpPeerType::Global)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../lab/fixtures/frr-8.4-two-router.bin");
        std::fs::read(path).expect("the lab fixture is committed")
    }

    #[test]
    fn splits_the_committed_fixture_into_ten_frames() {
        let stream = fixture();
        let spans = split(&stream).unwrap();
        assert_eq!(spans.len(), 10);
        assert_eq!(
            spans.first().map(|s| s.msg_type),
            Some(4),
            "first frame is Initiation"
        );
        assert_eq!(spans.iter().filter(|s| s.msg_type == 0).count(), 8);
        assert_eq!(spans.iter().filter(|s| s.msg_type == 3).count(), 1);
        let total: usize = spans.iter().map(|s| s.len).sum();
        assert_eq!(total, stream.len(), "spans tile the stream exactly");
    }

    #[test]
    fn classifies_initiation_peer_up_and_routes() {
        let frames = frames(&fixture()).unwrap();
        let init = frames
            .iter()
            .find_map(|f| match &f.kind {
                FrameKind::Initiation { sys_name, .. } => Some(sys_name.clone()),
                _ => None,
            })
            .expect("the capture has an Initiation message");
        assert_eq!(init.as_deref(), Some("r2"));

        let peer_up = frames
            .iter()
            .find_map(|f| match &f.kind {
                FrameKind::PeerUp {
                    peer_asn,
                    local_asn,
                    families,
                    ..
                } => Some((*peer_asn, *local_asn, families.clone())),
                _ => None,
            })
            .expect("the capture has a Peer Up message");
        assert_eq!(peer_up.0, 65001);
        assert_eq!(peer_up.1, 65002);
        assert!(!peer_up.2.is_empty(), "Peer Up declares families");

        let routing: Vec<_> = frames
            .iter()
            .filter_map(|f| match &f.kind {
                FrameKind::RouteMonitoring {
                    is_eor, asn_len, ..
                } => Some((*is_eor, *asn_len)),
                _ => None,
            })
            .collect();
        assert_eq!(routing.len(), 8);
        assert_eq!(
            routing.iter().filter(|(eor, _)| *eor).count(),
            4,
            "four EoR markers"
        );
        assert!(routing.iter().all(|(_, len)| *len == AsnLength::Bits32));
    }

    #[test]
    fn the_pdu_offset_lands_on_a_bgp_header() {
        let frames = frames(&fixture()).unwrap();
        for frame in frames {
            if let FrameKind::RouteMonitoring { pdu_offset, .. } = frame.kind {
                // A BGP message starts with sixteen 0xFF bytes of marker.
                let marker = &frame.bytes[pdu_offset..pdu_offset + 16];
                assert!(
                    marker.iter().all(|b| *b == 0xFF),
                    "frame at {} does not start its PDU with a BGP marker",
                    frame.span.offset
                );
                let declared = u16::from_be_bytes([
                    frame.bytes[pdu_offset + 16],
                    frame.bytes[pdu_offset + 17],
                ]) as usize;
                assert_eq!(
                    frame.bytes.len() - pdu_offset,
                    declared,
                    "the PDU length field matches the bytes carried"
                );
            }
        }
    }

    #[test]
    fn a_truncated_stream_is_an_error_not_a_short_list() {
        let stream = fixture();
        let cut = &stream[..stream.len() - 20];
        let err = split(cut).unwrap_err();
        assert!(matches!(err, SplitError::TruncatedBody { .. }));
    }

    #[test]
    fn the_stream_splitter_reassembles_a_dribbled_capture() {
        let stream = fixture();
        let mut splitter = StreamSplitter::new();
        let mut frames = 0usize;
        // Three bytes at a time forces frames to straddle chunk boundaries.
        for chunk in stream.chunks(3) {
            splitter.push(chunk);
            while let Some(frame) = splitter.next_frame().unwrap() {
                assert_eq!(frame[0], 3);
                frames += 1;
            }
        }
        assert_eq!(frames, 10);
        assert_eq!(splitter.buffered(), 0, "nothing is left over");
    }

    #[test]
    fn the_stream_splitter_refuses_a_wrong_version_immediately() {
        let mut splitter = StreamSplitter::new();
        splitter.push(&[4, 0, 0, 0, 6, 4]);
        assert!(matches!(
            splitter.next_frame(),
            Err(SplitError::UnsupportedVersion { version: 4, .. })
        ));
    }

    #[test]
    fn a_non_v3_version_is_refused() {
        let mut stream = fixture();
        stream[0] = 4;
        let err = split(&stream).unwrap_err();
        assert!(matches!(
            err,
            SplitError::UnsupportedVersion { version: 4, .. }
        ));
    }
}

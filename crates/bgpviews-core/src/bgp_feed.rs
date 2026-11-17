//! The BGP feed: one passive session, archived the same way a BMP stream is.
//!
//! Design reference: `docs/design.md` section 4, "The BGP peering feed". A router
//! that cannot export BMP can still peer with bgpviews, and what it sends us is its
//! Adj-RIB-In after its own import policy. The collector never announces a route, so
//! this is a read-only vantage point, not a route server.
//!
//! The feed is also where the baseline question gets its best answer: as a BGP peer
//! we receive the peer's **own** end-of-RIB marker, which is exactly the evidence BMP
//! cannot deliver for a peer that came up after the collector attached.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};

use bgpkit_parser::models::{AsnLength, BgpMessage};
use bgpkit_parser::parser::bgp::parse_bgp_message;
use bytes::Bytes;

use crate::collector::{Collector, CollectorError};

/// BGP marker: sixteen 0xFF bytes.
const MARKER: [u8; 16] = [0xFF; 16];
/// Fixed BGP header length.
const HEADER_LEN: usize = 19;
/// Largest BGP message we will buffer.
const MAX_MESSAGE: usize = 4096;
/// Hold time we advertise, in seconds. RFC 4271 says the keepalive interval is a
/// third of it.
const HOLD_TIME: u16 = 90;

/// Why a BGP feed session failed.
#[derive(Debug)]
pub enum BgpFeedError {
    /// Socket read or write failed.
    Io(std::io::Error),
    /// A message could not be parsed.
    Parse(String),
    /// The peer sent something the feed does not accept.
    Refused(String),
    /// The collector refused a record.
    Collector(CollectorError),
}

impl std::fmt::Display for BgpFeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "bgp feed io: {e}"),
            Self::Parse(detail) => write!(f, "bgp feed parse: {detail}"),
            Self::Refused(detail) => write!(f, "bgp feed refused: {detail}"),
            Self::Collector(e) => write!(f, "bgp feed collector: {e}"),
        }
    }
}

impl std::error::Error for BgpFeedError {}

impl From<std::io::Error> for BgpFeedError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<CollectorError> for BgpFeedError {
    fn from(value: CollectorError) -> Self {
        Self::Collector(value)
    }
}

/// What the feed needs to know about itself.
#[derive(Debug, Clone)]
pub struct FeedConfig {
    /// Our AS on this session.
    pub local_as: u32,
    /// Our address on this session, used for the BGP4MP envelope.
    pub local_ip: IpAddr,
    /// Our BGP identifier, when the configuration names one. A `bgp` client must, because it
    /// originates the session; without it the identifier falls back to `local_ip`.
    pub bgp_id: Option<std::net::Ipv4Addr>,
    /// The peer's address, which the router knows as its neighbour.
    pub peer_ip: IpAddr,
    /// Hard ceiling on prefixes per family, from `max_prefixes` in the config.
    pub max_prefixes: Option<u64>,
}

/// What the peer agreed to.
#[derive(Debug, Clone, Copy)]
pub struct Negotiated {
    /// Width of the AS numbers inside the UPDATEs.
    pub asn_len: AsnLength,
}

/// Serve one BGP session, feeding records into `collector` until the peer closes.
///
/// # Errors
/// Returns [`BgpFeedError`] when the handshake fails, a message cannot be parsed, or
/// the collector refuses a record.
pub fn serve(
    mut stream: TcpStream,
    config: &FeedConfig,
    collector: &mut Collector,
    clock: &mut dyn FnMut() -> i64,
) -> Result<Negotiated, BgpFeedError> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(u64::from(HOLD_TIME))))?;
    stream.set_nodelay(true)?;

    // RFC 4271 8.2: both sides send OPEN before anything else.
    stream.write_all(&our_open(config))?;
    let peer_open = read_message(&mut stream)?;
    let negotiated = negotiate(&peer_open, config)?;
    stream.write_all(&keepalive())?;

    loop {
        // A peer that closes, resets or goes silent past the hold time has ended the
        // session. That is a normal outcome, not a failure: the caller publishes the
        // open segment as partial with `session_ended`.
        let raw = match read_message(&mut stream) {
            Ok(raw) => raw,
            Err(BgpFeedError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(negotiated)
            }
            Err(e) => return Err(e),
        };
        let parsed = parse_bgp_message(
            &mut Bytes::copy_from_slice(&raw),
            false,
            &negotiated.asn_len,
        )
        .map_err(|e| BgpFeedError::Parse(e.to_string()))?;
        let at = clock();
        match parsed {
            BgpMessage::KeepAlive => {}
            // A peer that shuts the session down administratively has ended it, not broken
            // it: `session_ended` is the honest reason, and the segment it leaves is as real
            // as one closed by a TCP FIN.
            BgpMessage::Notification(_) => return Ok(negotiated),
            BgpMessage::Update(update) => {
                let is_eor = update.is_end_of_rib();
                collector.on_bgp_update(
                    crate::collector::BgpRecord {
                        peer_ip: config.peer_ip,
                        peer_asn: peer_as_u32(&peer_open),
                        local_asn: config.local_as,
                        local_ip: config.local_ip,
                        asn_len: negotiated.asn_len,
                        is_eor,
                        eor_families: if is_eor {
                            families_of_eor(&update, config)
                        } else {
                            Vec::new()
                        },
                    },
                    &raw,
                    at,
                )?;
            }
            BgpMessage::Open(_) | BgpMessage::RouteRefresh(_) => {
                return Err(BgpFeedError::Refused(
                    "second OPEN or a route refresh is not accepted".to_string(),
                ))
            }
        }
    }
}

/// Read one whole BGP message, including its 19-byte header.
fn read_message(stream: &mut TcpStream) -> Result<Vec<u8>, BgpFeedError> {
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header)?;
    if header[..16] != MARKER {
        return Err(BgpFeedError::Refused("missing BGP marker".to_string()));
    }
    let declared = u16::from_be_bytes([header[16], header[17]]) as usize;
    if !(HEADER_LEN..=MAX_MESSAGE).contains(&declared) {
        return Err(BgpFeedError::Refused(format!(
            "message length {declared} is outside {HEADER_LEN}..{MAX_MESSAGE}"
        )));
    }
    let mut raw = Vec::with_capacity(declared);
    raw.extend_from_slice(&header);
    raw.resize(declared, 0);
    stream.read_exact(&mut raw[HEADER_LEN..])?;
    Ok(raw)
}

fn keepalive() -> Vec<u8> {
    let mut message = Vec::with_capacity(HEADER_LEN);
    message.extend_from_slice(&MARKER);
    message.extend_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    message.push(4); // type KEEPALIVE
    message
}

/// Our OPEN: four-octet AS capability when needed, IPv4 unicast, no optional
/// parameters beyond those, and nothing that implies we will send routes.
fn our_open(config: &FeedConfig) -> Vec<u8> {
    let four_byte = config.local_as > 65_535;
    let asn_field: u16 = if four_byte {
        23456 // AS_TRANS
    } else {
        config.local_as as u16
    };
    let router_id = match (config.bgp_id, config.local_ip) {
        (Some(id), _) => id.octets(),
        (None, IpAddr::V4(v4)) => v4.octets(),
        (None, IpAddr::V6(_)) => [0, 0, 0, 1],
    };

    let mut capabilities: Vec<u8> = Vec::new();
    // Capability 1: multiprotocol, AFI 1 SAFI 1 and AFI 2 SAFI 1. Both families are
    // advertised because the feed archives both; announcing only IPv4 told a peer with an
    // IPv6 channel that we would not take its IPv6 routes, which is exactly what BIRD
    // reported as "AF announced: ipv4" on the second duck run.
    capabilities.extend_from_slice(&[1, 4, 0, 1, 0, 1]);
    capabilities.extend_from_slice(&[1, 4, 0, 2, 0, 1]);
    // Capability 65 is advertised by every speaker that understands four-octet AS numbers,
    // not only by the ones whose own AS needs it. RFC 6793 section 3: the capability says
    // what the speaker can parse. Omitting it because our AS fits in two bytes told a
    // 4-byte-AS peer (BIRD, AS 400644) that we cannot read its AS_PATH, and it refused the
    // session: the first thing duck found that the FRR lab could not.
    capabilities.extend_from_slice(&[65, 4]);
    capabilities.extend_from_slice(&config.local_as.to_be_bytes());

    let mut params = Vec::new();
    params.push(2u8); // optional parameter type 2: capabilities
    params.push(capabilities.len() as u8);
    params.extend_from_slice(&capabilities);

    let mut body = Vec::new();
    body.push(4); // version
    body.extend_from_slice(&asn_field.to_be_bytes());
    body.extend_from_slice(&HOLD_TIME.to_be_bytes());
    body.extend_from_slice(&router_id);
    body.push(params.len() as u8);
    body.extend_from_slice(&params);

    let total = HEADER_LEN + body.len();
    let mut message = Vec::with_capacity(total);
    message.extend_from_slice(&MARKER);
    message.extend_from_slice(&(total as u16).to_be_bytes());
    message.push(1); // type OPEN
    message.extend_from_slice(&body);
    message
}

fn negotiate(peer_open: &[u8], config: &FeedConfig) -> Result<Negotiated, BgpFeedError> {
    let parsed = parse_bgp_message(
        &mut Bytes::copy_from_slice(peer_open),
        false,
        &AsnLength::Bits32,
    )
    .map_err(|e| BgpFeedError::Parse(e.to_string()))?;
    let BgpMessage::Open(open) = parsed else {
        return Err(BgpFeedError::Refused(
            "first message was not an OPEN".to_string(),
        ));
    };
    if open.version != 4 {
        return Err(BgpFeedError::Refused(format!(
            "BGP version {} is not supported",
            open.version
        )));
    }
    // Four-octet support decides the BGP4MP subtype, which is a decoder contract, so
    // it has to come from the peer's capability list rather than from our config.
    let four_byte = open.opt_params.iter().any(|param| {
        matches!(&param.param_value, bgpkit_parser::models::ParamValue::Capacities(caps)
            if caps.iter().any(|cap| u8::from(cap.ty) == 65))
    });
    let _ = (config, &open.opt_params);
    Ok(Negotiated {
        asn_len: if four_byte {
            AsnLength::Bits32
        } else {
            AsnLength::Bits16
        },
    })
}

/// The peer's AS, from the OPEN and, when it sent AS_TRANS, from the capability.
fn peer_as_u32(peer_open: &[u8]) -> u32 {
    let Ok(parsed) = parse_bgp_message(
        &mut Bytes::copy_from_slice(peer_open),
        false,
        &AsnLength::Bits32,
    ) else {
        return 0;
    };
    let BgpMessage::Open(open) = parsed else {
        return 0;
    };
    let announced = u32::from(open.asn);
    if announced != 23456 {
        return announced;
    }
    // The parser hands capability 65 back as a typed value, not as raw bytes: matching only
    // `CapabilityValue::Raw` silently recorded the peer as AS_TRANS (23456) against a real
    // BIRD, which is what the first duck run archived. Both shapes are accepted so a parser
    // change cannot quietly reintroduce it.
    for param in &open.opt_params {
        if let bgpkit_parser::models::ParamValue::Capacities(caps) = &param.param_value {
            for cap in caps {
                if u8::from(cap.ty) != 65 {
                    continue;
                }
                match &cap.value {
                    bgpkit_parser::models::CapabilityValue::FourOctetAs(four) => return four.asn,
                    bgpkit_parser::models::CapabilityValue::Raw(raw) if raw.len() == 4 => {
                        return u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                    }
                    _ => {}
                }
            }
        }
    }
    announced
}

/// Families an end-of-RIB marker closes. A classic marker names IPv4 unicast, and an
/// MP_UNREACH marker names its own family.
fn families_of_eor(
    update: &bgpkit_parser::models::BgpUpdateMessage,
    _config: &FeedConfig,
) -> Vec<(u16, u16)> {
    use bgpkit_parser::models::AttributeValue;
    for attribute in &update.attributes {
        if let AttributeValue::MpUnreachNlri(nlri) = attribute {
            return vec![(u16::from(nlri.afi), u16::from(u8::from(nlri.safi)))];
        }
    }
    vec![(1, 1)]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::collector::{Collector, CollectorConfig};
    use std::net::{Ipv4Addr, TcpListener};

    fn message(kind: u8, body: &[u8]) -> Vec<u8> {
        let total = HEADER_LEN + body.len();
        let mut raw = Vec::with_capacity(total);
        raw.extend_from_slice(&MARKER);
        raw.extend_from_slice(&(total as u16).to_be_bytes());
        raw.push(kind);
        raw.extend_from_slice(body);
        raw
    }

    fn peer_open(asn: u32) -> Vec<u8> {
        let mut caps = vec![1u8, 4, 0, 1, 0, 1, 65, 4];
        caps.extend_from_slice(&asn.to_be_bytes());
        let mut body = vec![4u8];
        body.extend_from_slice(&(asn as u16).to_be_bytes());
        body.extend_from_slice(&90u16.to_be_bytes());
        body.extend_from_slice(&[192, 0, 2, 9]);
        body.push(caps.len() as u8 + 2);
        body.push(2);
        body.push(caps.len() as u8);
        body.extend_from_slice(&caps);
        message(1, &body)
    }

    /// An UPDATE that withdraws 192.0.2.0/24 and carries no attributes.
    fn withdraw_update() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&4u16.to_be_bytes());
        body.extend_from_slice(&[24, 192, 0, 2]);
        body.extend_from_slice(&0u16.to_be_bytes());
        message(2, &body)
    }

    fn eor_update() -> Vec<u8> {
        message(2, &[0, 0, 0, 0])
    }

    #[test]
    fn our_open_carries_the_configured_bgp_identifier() {
        let config = FeedConfig {
            local_as: 64512,
            bgp_id: Some(Ipv4Addr::new(192, 0, 2, 9)),
            local_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)),
            peer_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 6)),
            max_prefixes: None,
        };
        let open = our_open(&config);
        // Header is 19 bytes: marker, length, type. Then version, AS, hold time, identifier.
        assert_eq!(
            &open[24..28],
            &[192, 0, 2, 9],
            "the identifier is the configured one"
        );

        // Without it the address on the session stands in, which is what a bmp client has.
        let derived = our_open(&FeedConfig {
            bgp_id: None,
            ..config
        });
        assert_eq!(&derived[24..28], &[203, 0, 113, 5]);
    }

    #[test]
    fn a_peer_update_and_marker_become_a_segment_with_an_observed_baseline() {
        let root = std::env::temp_dir().join("bgpviews-bgp-feed");
        let _ = std::fs::remove_dir_all(&root);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let config = CollectorConfig {
            client: "duck".to_string(),
            view: "adj-in-pre".to_string(),
            feed: "bgp".to_string(),
            epoch: 1,
            interval_seconds: 900,
            rib: true,
            disk_warning_percent: 90,
            disk_critical_percent: 97,
            write_stall_ms: 2000,
            backpressure_deadline_seconds: 300,
            root: root.clone(),
        };
        let feed = FeedConfig {
            local_as: 400644,
            bgp_id: None,
            local_ip: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            peer_ip: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            max_prefixes: Some(1_000_000),
        };

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut collector = Collector::open_at(config, 1_794_702_169_000_000).unwrap();
            let mut clock = || 1_794_702_169_000_000i64;
            serve(stream, &feed, &mut collector, &mut clock).unwrap();
            collector.finish(1_794_702_169_060_000).unwrap();
            collector
        });

        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.write_all(&peer_open(65001)).unwrap();
        let mut header = [0u8; HEADER_LEN];
        client.read_exact(&mut header).unwrap();
        client.write_all(&keepalive()).unwrap();
        client.write_all(&withdraw_update()).unwrap();
        client.write_all(&eor_update()).unwrap();
        // Half-close and drain instead of dropping: an immediate close can reset the
        // connection before the server has read the bytes it already buffered, which
        // no real peer does.
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut sink = Vec::new();
        let _ = client.read_to_end(&mut sink);

        let collector = server.join().unwrap();
        let segments = collector.published();
        assert_eq!(segments.len(), 1, "one segment for one interval");
        assert_eq!(
            segments[0].sidecar.records, 2,
            "the withdrawal and the marker"
        );
        assert_eq!(segments[0].sidecar.end_of_rib_records, 1);
        let observed: Vec<_> = segments[0]
            .sidecar
            .baseline
            .iter()
            .filter(|b| b.baseline == "observed")
            .collect();
        assert_eq!(
            observed.len(),
            1,
            "IPv4 unicast marker observed: {:?}",
            segments[0].sidecar.baseline
        );
        assert_eq!((observed[0].afi, observed[0].safi), (1, 1));
        // Provenance, not decoration: a sidecar that says "bmp" for a BGP feed makes the
        // archive lie about where its records came from.
        assert_eq!(segments[0].sidecar.feed, "bgp");
        let _ = std::fs::remove_dir_all(&root);
    }
}

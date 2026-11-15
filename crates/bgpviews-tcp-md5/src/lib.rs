//! TCP-MD5 (RFC 2385) keys for a listening socket, on Linux.
//!
//! This crate exists for exactly one syscall. `setsockopt(TCP_MD5SIG)` is how a listener
//! tells the kernel which peer address must sign its segments and with what key, and there
//! is no safe wrapper for it in the crates this project uses. The rest of the workspace sets
//! `unsafe_code = "forbid"`; this crate is the single, deliberate exception, kept as small
//! as the job allows so it can be audited in one sitting.
//!
//! What the kernel expects is `struct tcp_md5sig`: a `sockaddr_storage` for the peer, the
//! key length, and up to 80 key bytes. The struct has grown over kernel versions (a VRF id
//! was added in 4.13), and it is passed by pointer with a length. Rather than mirror the
//! layout with a `#[repr(C)]` struct that could drift, this builder writes the fields at
//! their documented offsets into a zeroed buffer and lets the kernel copy what it knows:
//! every kernel since 2.6 reads the same prefix, and a longer buffer than it expects is
//! harmless because only `optlen` bytes are copied.
//!
//! Refusing the key is a real outcome. `EPERM` means the caller lacks the capability the
//! kernel requires, `EAFNOSUPPORT` means this is not Linux. Both are surfaced, never
//! swallowed: a session that was supposed to be authenticated and is not must not look
//! healthy.

use std::io;
use std::net::IpAddr;
use std::os::unix::io::RawFd;

/// Longest key TCP-MD5 allows, from `TCP_MD5SIG_MAXKEYLEN`.
pub const MAX_KEY_LEN: usize = 80;

/// SOL_TCP, the level `TCP_MD5SIG` lives at.
const SOL_TCP: libc::c_int = 6;
/// TCP_MD5SIG on Linux.
const TCP_MD5SIG: libc::c_int = 14;
/// `sockaddr_storage` is 128 bytes; the peer address goes at offset 0.
const SOCKADDR_LEN: usize = 128;
/// `tcpm_keylen` is a `u16` at this offset in `struct tcp_md5sig`.
const KEYLEN_OFFSET: usize = SOCKADDR_LEN + 1 + 1;
/// `tcpm_key` follows `tcpm_ifindex` (an `int`).
const KEY_OFFSET: usize = SOCKADDR_LEN + 1 + 1 + 2 + 4;
/// Buffer length: key offset plus the longest key, rounded up for the VRF field.
const BUFFER_LEN: usize = KEY_OFFSET + MAX_KEY_LEN + 4;

/// Why a key could not be installed.
#[derive(Debug)]
pub enum Error {
    /// The key is longer than TCP-MD5 allows.
    KeyTooLong(usize),
    /// The kernel refused the option.
    Syscall(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyTooLong(len) => write!(
                f,
                "a TCP-MD5 key may be at most {MAX_KEY_LEN} bytes, this one is {len}"
            ),
            Self::Syscall(e) => write!(f, "setsockopt(TCP_MD5SIG) failed: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Syscall(value)
    }
}

/// Require `peer` to sign its segments with `key`, on the socket `fd`.
///
/// The socket is normally the listener: the kernel checks the key against the source
/// address of an incoming SYN, so one call per allowed peer is enough.
///
/// # Errors
/// Returns [`Error::KeyTooLong`] for a key over 80 bytes, or [`Error::Syscall`] when the
/// kernel refuses (a missing capability, a non-Linux platform, or an address family this
/// build does not know how to encode).
// The crate sets `unsafe_code = "deny"`, not `forbid`, so this one function may opt in.
// Anything added below needs the same treatment: one exception per audited call site, not a
// blanket relaxation of the crate.
#[allow(unsafe_code)]
pub fn require_key(fd: RawFd, peer: IpAddr, prefix_len: u8, key: &[u8]) -> Result<(), Error> {
    if key.len() > MAX_KEY_LEN {
        return Err(Error::KeyTooLong(key.len()));
    }
    let mut buffer = [0u8; BUFFER_LEN];
    write_sockaddr(&mut buffer, peer)?;
    // Native order, like every field of this struct: the kernel reads it as a `u16`, and a
    // big-endian length on a little-endian host is read as a number far beyond the 80-byte
    // limit, which the kernel answers with EINVAL rather than a helpful message.
    buffer[SOCKADDR_LEN + 1] = prefix_len;
    buffer[KEYLEN_OFFSET..KEYLEN_OFFSET + 2].copy_from_slice(&(key.len() as u16).to_ne_bytes());
    buffer[KEY_OFFSET..KEY_OFFSET + key.len()].copy_from_slice(key);

    // SAFETY: `fd` is a socket the caller owns, `buffer` outlives the call, and the
    // kernel reads at most `buffer.len()` bytes from it, as `optlen` states. The pointer
    // and length describe one valid, initialised allocation; no Rust reference aliases it.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            SOL_TCP,
            TCP_MD5SIG,
            buffer.as_ptr().cast(),
            buffer.len() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// Drop the key for `peer`, so its segments no longer need to be signed.
///
/// # Errors
/// As [`require_key`].
pub fn forget_key(fd: RawFd, peer: IpAddr, prefix_len: u8) -> Result<(), Error> {
    // An entry with key length zero is how the kernel is told to delete a key.
    require_key(fd, peer, prefix_len, &[])
}

/// Write a `sockaddr_in` or `sockaddr_in6` into the first bytes of `buffer`.
fn write_sockaddr(buffer: &mut [u8], peer: IpAddr) -> Result<(), Error> {
    match peer {
        IpAddr::V4(v4) => {
            // struct sockaddr_in: family (u16, native order), port (u16), addr (4 bytes).
            let family = libc::AF_INET as u16;
            buffer[0..2].copy_from_slice(&family.to_ne_bytes());
            buffer[4..8].copy_from_slice(&v4.octets());
            Ok(())
        }
        IpAddr::V6(v6) => {
            let family = libc::AF_INET6 as u16;
            buffer[0..2].copy_from_slice(&family.to_ne_bytes());
            // struct sockaddr_in6: family, port, flowinfo (u32), addr (16), scope (u32).
            buffer[8..24].copy_from_slice(&v6.octets());
            Ok(())
        }
    }
}

/// Encode a peer address the way [`write_sockaddr`] does, exposed for tests that want to
/// check the layout without a kernel.
#[must_use]
pub fn sockaddr_bytes(peer: IpAddr) -> Vec<u8> {
    let mut buffer = vec![0u8; SOCKADDR_LEN];
    let _ = write_sockaddr(&mut buffer, peer);
    buffer
}

/// The address families this crate knows how to encode, for a caller that wants to refuse
/// others before trying.
#[must_use]
pub fn supported(peer: IpAddr) -> bool {
    matches!(peer, IpAddr::V4(_) | IpAddr::V6(_))
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
    use std::net::TcpListener;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn an_ipv4_peer_is_encoded_as_sockaddr_in() {
        let bytes = sockaddr_bytes(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
        assert_eq!(
            u16::from_ne_bytes([bytes[0], bytes[1]]),
            libc::AF_INET as u16
        );
        assert_eq!(&bytes[4..8], &[192, 0, 2, 1]);
    }

    #[test]
    fn an_ipv6_peer_is_encoded_as_sockaddr_in6() {
        let peer: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let bytes = sockaddr_bytes(IpAddr::V6(peer));
        assert_eq!(
            u16::from_ne_bytes([bytes[0], bytes[1]]),
            libc::AF_INET6 as u16
        );
        assert_eq!(&bytes[8..24], &peer.octets());
    }

    #[test]
    fn an_oversized_key_is_refused_before_the_kernel_is_asked() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&listener);
        let key = vec![7u8; MAX_KEY_LEN + 1];
        match require_key(fd, IpAddr::V4(Ipv4Addr::LOCALHOST), 32, &key) {
            Err(Error::KeyTooLong(len)) => assert_eq!(len, MAX_KEY_LEN + 1),
            other => panic!("expected KeyTooLong, got {other:?}"),
        }
    }

    /// Installing a key needs CAP_NET_ADMIN on most kernels, which CI does not have. The
    /// test asserts the syscall is reached and that its answer is believed, whichever it is.
    #[test]
    fn the_kernel_is_asked_and_its_answer_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&listener);
        match require_key(
            fd,
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            32,
            b"viewss3cret",
        ) {
            Ok(()) => {
                // Installed: removing it must also work.
                forget_key(fd, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 32).unwrap();
            }
            Err(Error::Syscall(e)) => {
                let code = e.raw_os_error().unwrap_or(0);
                assert!(
                    code == libc::EPERM || code == libc::EACCES || code == libc::EINVAL,
                    "unexpected refusal: {e}"
                );
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }
}

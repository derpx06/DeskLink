//! The carrier underneath a [`SessionLink`](super::SessionLink).
//!
//! Pairing, identity and signed signaling all travel over "the link". Until now
//! that was always an `SslStream<TcpStream>`, welded into the struct. Bluetooth
//! RFCOMM and a punched UDP socket are the same link to everything above this
//! boundary, so the socket becomes an implementation detail behind this trait
//! and `SessionLink` keeps the parts that do not vary — the pinned certificate,
//! the DER keys, and the peer's `DeviceInfo`.

use std::net::IpAddr;

use crate::device_links::packet::NetworkPacket;

/// The outcome of one read attempt.
///
/// Three arms rather than an `io::Result<usize>` because the distinction that
/// matters — "quiet" versus "dead" — is only expressible by the transport. On a
/// socket it is an `EAGAIN`/`ETIMEDOUT` split (see `is_idle_read`); an
/// in-memory queue has no errno, and neither will RFCOMM. Deciding it here,
/// once per implementation, is the whole point of the boundary.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    Byte(u8),
    /// Nothing arrived within the transport's idle window. The reader loops.
    Idle,
    /// The peer closed cleanly.
    Closed,
}

pub trait LinkTransport: Send + Sync {
    /// Writes one newline-framed packet, atomically with respect to any other
    /// sender on the same link.
    fn send_packet(&self, packet: &NetworkPacket) -> Result<(), String>;

    /// Reads at most one byte, blocking no longer than the transport's idle
    /// window.
    ///
    /// `Err` means the link is dead, not merely quiet.
    fn read_byte(&self) -> Result<ReadOutcome, String>;

    /// Best-effort immediate shutdown.
    ///
    /// Must unblock a reader parked in [`read_byte`](Self::read_byte) from
    /// another thread. `DeviceManager::register_link` closes the link it is
    /// replacing precisely to stop the superseded generation's reader at once;
    /// an implementation that merely sets a flag and waits for the reader to
    /// notice widens that race silently.
    ///
    /// Idempotency is `SessionLink`'s job, not the implementation's.
    fn shutdown(&self);

    /// The local address to seed ICE host candidates from.
    ///
    /// `None` is correct — and required — whenever this transport is not the
    /// same L3 path the media will take. A Bluetooth or relayed link must
    /// return `None` rather than a local address that has no route to the peer:
    /// a host candidate that cannot work costs ICE its full timeout before it
    /// is abandoned.
    fn local_ip(&self) -> Option<IpAddr> {
        None
    }
}

/// A link with no carrier at all, for tests that only exercise session state.
///
/// Replaces the `Option` that used to wrap the stream purely so
/// `SessionLink::test_placeholder` could exist. Its error text is the string
/// those tests already assert on, and one `should_surface_send_error`
/// deliberately suppresses.
#[cfg(test)]
pub struct DeadTransport;

#[cfg(test)]
impl LinkTransport for DeadTransport {
    fn send_packet(&self, _packet: &NetworkPacket) -> Result<(), String> {
        Err("Session link has no network stream".to_string())
    }

    fn read_byte(&self) -> Result<ReadOutcome, String> {
        Err("Session link has no network stream".to_string())
    }

    fn shutdown(&self) {}
}

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::link_transport::{LinkTransport, ReadOutcome};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::NetworkPacket;
use crate::device_links::pairing::PairingHandler;

pub struct SessionLink {
    /// Swappable so a replacement socket can be adopted in place. See
    /// [`SessionLink::adopt`].
    transport: Mutex<Arc<dyn LinkTransport>>,
    pub certificate_pem: String,
    pub local_public_der: Vec<u8>,
    pub remote_public_der: Vec<u8>,
    pub info: DeviceInfo,
    closed: AtomicBool,
}

impl SessionLink {
    pub fn new(
        transport: Arc<dyn LinkTransport>,
        certificate_pem: String,
        local_public_der: Vec<u8>,
        remote_public_der: Vec<u8>,
        info: DeviceInfo,
    ) -> Self {
        Self {
            transport: Mutex::new(transport),
            certificate_pem,
            local_public_der,
            remote_public_der,
            info,
            closed: AtomicBool::new(false),
        }
    }

    /// The live carrier for this link.
    ///
    /// A closed link is reported as an error rather than handed out. The
    /// session deliberately keeps a closed link installed while an
    /// authenticated WebRTC transport survives the loss of the bootstrap
    /// socket, so without this check pairing packets and signed signaling were
    /// written into a dead stream and silently lost.
    ///
    /// **This is the only place allowed to produce `"Session link is closed"`.**
    /// `transport::should_surface_send_error` matches that text to tell a
    /// routine transport transition from a real feature failure; an
    /// implementation inventing its own wording would surface as a UI banner
    /// once per second from the clipboard poller.
    fn transport(&self) -> Result<Arc<dyn LinkTransport>, String> {
        if self.is_closed() {
            return Err("Session link is closed".to_string());
        }
        // Cloned, not borrowed: reads block for as long as the peer is quiet,
        // and holding this lock across one would stall every sender.
        Ok(Arc::clone(&self.carrier()))
    }

    fn carrier(&self) -> Arc<dyn LinkTransport> {
        Arc::clone(
            &self
                .transport
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Take over a replacement socket without changing this link's identity.
    ///
    /// A `SessionBinding` is keyed on this `Arc<SessionLink>`, so re-creating
    /// the link for a replacement socket makes every WebRTC callback bound to
    /// it stale -- and `register_link` then clears the peer and handover it was
    /// serving. That destroyed a *working* transport every time the phone
    /// refreshed its bootstrap socket: the handover ran again, its own
    /// bootstrap socket dropped again, and the device sat on "Connecting"
    /// forever. Android has kept its `retainedBinding` across exactly this
    /// event since `onConnectionReceived` learned to; this is the desktop half.
    ///
    /// The caller must ensure the outgoing socket is closed, so that no reader
    /// thread is still framing packets out of it.
    pub fn adopt(&self, transport: Arc<dyn LinkTransport>) {
        *self
            .transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = transport;
        self.closed.store(false, Ordering::Release);
    }

    pub fn send_packet(&self, packet: &NetworkPacket) -> Result<(), String> {
        self.transport()?.send_packet(packet)
    }

    pub fn read_byte(&self) -> Result<ReadOutcome, String> {
        self.transport()?.read_byte()
    }

    /// The local address to seed ICE host candidates from, when this link is
    /// the same L3 path the media will take. `None` otherwise, and on a closed
    /// link.
    pub fn local_ip(&self) -> Option<IpAddr> {
        self.transport().ok()?.local_ip()
    }

    pub fn verification_key(&self, timestamp: i64) -> String {
        PairingHandler::verification_key(&self.local_public_der, &self.remote_public_der, timestamp)
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.carrier().shutdown();
    }

    /// Whether this link has been closed.
    ///
    /// Deliberately the local flag rather than a question for the transport:
    /// `has_live_bootstrap_link` drives `wants_bootstrap_connection` and
    /// therefore discovery, so a transport reporting "closed" for a transient
    /// condition would make the daemon redial in a loop.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn test_placeholder(device_id: &str) -> Self {
        Self::for_test(device_id, Arc::new(super::link_transport::DeadTransport))
    }

    #[cfg(test)]
    pub fn for_test(device_id: &str, transport: Arc<dyn LinkTransport>) -> Self {
        Self {
            transport: Mutex::new(transport),
            certificate_pem: String::new(),
            local_public_der: Vec::new(),
            remote_public_der: Vec::new(),
            info: DeviceInfo {
                id: device_id.to_string(),
                name: device_id.to_string(),
                device_type: "phone".to_string(),
                protocol_version: crate::device_links::packet::PROTOCOL_VERSION,
                incoming_capabilities: Vec::new(),
                outgoing_capabilities: Vec::new(),
            },
            closed: AtomicBool::new(false),
        }
    }
}

impl std::fmt::Debug for SessionLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionLink")
            .field("device_id", &self.info.id)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

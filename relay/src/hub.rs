//! Who is online, who allows whom, and where their frames go.
//!
//! The hub holds no sockets: each connection hands it an [`Outbox`] and drains
//! the matching [`Inbox`] on its own thread. That keeps every mutuality and
//! presence rule testable with plain channels.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use serde_json::Value;

use crate::protocol::{ErrorCode, ServerMessage, CLOSE_RATE_LIMITED, CLOSE_REPLACED};

/// Forwarded bytes allowed to wait for one slow receiver. Past this the
/// sender is told the peer is offline instead of the relay buffering for it.
pub const MAX_QUEUED_BYTES: usize = 2 * 1024 * 1024;
/// Extra room for control frames (presence) above [`MAX_QUEUED_BYTES`].
/// A receiver that falls this far behind is closed with 4429 rather than
/// letting flapping peers grow its queue without bound.
pub const CONTROL_HEADROOM: usize = 256 * 1024;

#[derive(Debug, PartialEq)]
pub enum Outbound {
    Frame(String),
    Close(u16, &'static str),
}

#[derive(Clone)]
pub struct Outbox {
    tx: Sender<Outbound>,
    queued: Arc<AtomicUsize>,
}

pub struct Inbox {
    rx: Receiver<Outbound>,
    queued: Arc<AtomicUsize>,
}

pub fn outbox() -> (Outbox, Inbox) {
    let (tx, rx) = channel();
    let queued = Arc::new(AtomicUsize::new(0));
    (
        Outbox {
            tx,
            queued: queued.clone(),
        },
        Inbox { rx, queued },
    )
}

impl Outbox {
    /// Forwarded payloads (`bounded`) stop at [`MAX_QUEUED_BYTES`]; control
    /// frames may use [`CONTROL_HEADROOM`] beyond that, and past it the
    /// receiver is told to close.
    fn push(&self, message: &ServerMessage, bounded: bool) -> bool {
        let Ok(text) = serde_json::to_string(message) else {
            return false;
        };
        let queued = self.queued.load(Ordering::Relaxed) + text.len();
        if bounded && queued > MAX_QUEUED_BYTES {
            return false;
        }
        if queued > MAX_QUEUED_BYTES + CONTROL_HEADROOM {
            self.close(CLOSE_RATE_LIMITED, "too far behind");
            return false;
        }
        self.queued.fetch_add(text.len(), Ordering::Relaxed);
        self.tx.send(Outbound::Frame(text)).is_ok()
    }

    fn close(&self, code: u16, reason: &'static str) {
        let _ = self.tx.send(Outbound::Close(code, reason));
    }
}

impl Inbox {
    pub fn try_recv(&self) -> Option<Outbound> {
        let next = self.rx.try_recv().ok()?;
        if let Outbound::Frame(text) = &next {
            self.queued.fetch_sub(text.len(), Ordering::Relaxed);
        }
        Some(next)
    }
}

struct Peer {
    conn: u64,
    outbox: Outbox,
    allow: HashSet<String>,
}

#[derive(Default)]
pub struct Hub {
    peers: HashMap<String, Peer>,
}

impl Hub {
    pub fn online(&self) -> usize {
        self.peers.len()
    }

    /// Calls from a replaced connection are ignored, so its last frames
    /// cannot act under an identity that now belongs to the new socket.
    fn current(&self, fp: &str, conn: u64) -> Option<&Peer> {
        self.peers.get(fp).filter(|peer| peer.conn == conn)
    }

    /// Mutual means both online and each in the other's allow set. An
    /// offline device has no allow set, so it is never mutual with anyone.
    fn mutual(&self, a: &str, b: &str) -> bool {
        match (self.peers.get(a), self.peers.get(b)) {
            (Some(pa), Some(pb)) => a != b && pa.allow.contains(b) && pb.allow.contains(a),
            _ => false,
        }
    }

    fn mutual_peers(&self, fp: &str) -> HashSet<String> {
        self.peers
            .get(fp)
            .map(|peer| {
                peer.allow
                    .iter()
                    .filter(|other| self.mutual(fp, other))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn presence(&self, to: &str, about: &str, online: bool) {
        if let Some(peer) = self.peers.get(to) {
            peer.outbox.push(
                &ServerMessage::Presence {
                    fp: about.to_string(),
                    online,
                },
                false,
            );
        }
    }

    /// Newest connection wins: a previous socket for `fp` is told to close
    /// with 4409, and its mutual peers see it go offline. The new connection
    /// starts with an empty allow set and becomes visible again on `allow`.
    pub fn register(&mut self, fp: &str, conn: u64, outbox: Outbox) {
        if let Some(old) = self.peers.get(fp) {
            old.outbox
                .close(CLOSE_REPLACED, "replaced by a newer connection");
            self.remove(fp);
        }
        self.peers.insert(
            fp.to_string(),
            Peer {
                conn,
                outbox,
                allow: HashSet::new(),
            },
        );
    }

    pub fn unregister(&mut self, fp: &str, conn: u64) {
        if self.current(fp, conn).is_some() {
            self.remove(fp);
        }
    }

    fn remove(&mut self, fp: &str) {
        for peer in self.mutual_peers(fp) {
            self.presence(&peer, fp, false);
        }
        self.peers.remove(fp);
    }

    /// Replaces the allow set and announces every mutuality change to both
    /// sides. Losing mutuality is announced as `online: false` to the two
    /// devices that were mutual a moment ago, which tells neither anything new.
    pub fn set_allow(&mut self, fp: &str, conn: u64, fps: Vec<String>) {
        if self.current(fp, conn).is_none() {
            return;
        }
        let before = self.mutual_peers(fp);
        if let Some(peer) = self.peers.get_mut(fp) {
            peer.allow = fps.into_iter().filter(|other| other != fp).collect();
        }
        let after = self.mutual_peers(fp);
        for gained in after.difference(&before) {
            self.presence(fp, gained, true);
            self.presence(gained, fp, true);
        }
        for lost in before.difference(&after) {
            self.presence(fp, lost, false);
            self.presence(lost, fp, false);
        }
    }

    /// `not-allowed` only when the sender itself has not allowed `to`.
    /// Everything else that prevents delivery reads as `offline`, so a device
    /// that is not mutual cannot probe whether the other is online.
    pub fn forward(&self, from: &str, conn: u64, to: &str, data: Value) -> Result<(), ErrorCode> {
        let Some(sender) = self.current(from, conn) else {
            return Err(ErrorCode::Offline);
        };
        if !sender.allow.contains(to) {
            return Err(ErrorCode::NotAllowed);
        }
        let target = match self.peers.get(to) {
            Some(target) if self.mutual(from, to) => target,
            _ => return Err(ErrorCode::Offline),
        };
        let message = ServerMessage::Msg {
            from: from.to_string(),
            data,
        };
        if target.outbox.push(&message, true) {
            Ok(())
        } else {
            Err(ErrorCode::Offline)
        }
    }

    pub fn can_turn(&self, from: &str, conn: u64, peer: &str) -> bool {
        self.current(from, conn).is_some() && self.mutual(from, peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn join(hub: &mut Hub, fp: &str, conn: u64) -> Inbox {
        let (outbox, inbox) = outbox();
        hub.register(fp, conn, outbox);
        inbox
    }

    fn drain(inbox: &Inbox) -> Vec<Outbound> {
        std::iter::from_fn(|| inbox.try_recv()).collect()
    }

    fn presence(fp: &str, online: bool) -> Outbound {
        Outbound::Frame(
            serde_json::to_string(&ServerMessage::Presence {
                fp: fp.into(),
                online,
            })
            .unwrap(),
        )
    }

    fn allow(hub: &mut Hub, fp: &str, conn: u64, fps: &[&str]) {
        hub.set_allow(fp, conn, fps.iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn presence_only_once_both_sides_allow() {
        let mut hub = Hub::default();
        let a = join(&mut hub, "A", 1);
        let b = join(&mut hub, "B", 2);
        allow(&mut hub, "A", 1, &["B"]);
        assert!(drain(&a).is_empty() && drain(&b).is_empty());
        allow(&mut hub, "B", 2, &["A"]);
        assert_eq!(drain(&a), vec![presence("B", true)]);
        assert_eq!(drain(&b), vec![presence("A", true)]);
        // Re-sending the same set changes nothing and announces nothing.
        allow(&mut hub, "B", 2, &["A"]);
        assert!(drain(&a).is_empty() && drain(&b).is_empty());
    }

    #[test]
    fn revoking_and_disconnecting_announce_offline_to_mutual_peers_only() {
        let mut hub = Hub::default();
        let a = join(&mut hub, "A", 1);
        let b = join(&mut hub, "B", 2);
        let c = join(&mut hub, "C", 3);
        allow(&mut hub, "A", 1, &["B"]);
        allow(&mut hub, "B", 2, &["A"]);
        // C allows A, but A does not allow C.
        allow(&mut hub, "C", 3, &["A"]);
        drain(&a);
        drain(&b);
        assert!(drain(&c).is_empty());

        allow(&mut hub, "B", 2, &[]);
        assert_eq!(drain(&a), vec![presence("B", false)]);
        assert_eq!(drain(&b), vec![presence("A", false)]);

        allow(&mut hub, "B", 2, &["A"]);
        drain(&a);
        drain(&b);
        hub.unregister("A", 1);
        assert_eq!(drain(&b), vec![presence("A", false)]);
        assert!(drain(&c).is_empty());
        assert_eq!(hub.online(), 2);
    }

    #[test]
    fn forwarding_requires_mutual_allow_and_never_reveals_non_mutual_presence() {
        let mut hub = Hub::default();
        let a = join(&mut hub, "A", 1);
        let b = join(&mut hub, "B", 2);
        allow(&mut hub, "A", 1, &["B"]);
        assert_eq!(
            hub.forward("A", 1, "B", json!("x")),
            Err(ErrorCode::Offline),
            "B is online but has not allowed A; A must not learn that"
        );
        assert_eq!(
            hub.forward("A", 1, "Z", json!("x")),
            Err(ErrorCode::NotAllowed)
        );
        assert_eq!(
            hub.forward("A", 1, "nobody-allowed", json!("x")),
            Err(ErrorCode::NotAllowed)
        );
        allow(&mut hub, "A", 1, &["B", "Q"]);
        assert_eq!(
            hub.forward("A", 1, "Q", json!("x")),
            Err(ErrorCode::Offline),
            "a truly offline peer reads the same as a non-mutual one"
        );
        assert!(!hub.can_turn("A", 1, "B"));

        allow(&mut hub, "B", 2, &["A"]);
        drain(&a);
        drain(&b);
        hub.forward("A", 1, "B", json!({"sdp": "opaque"})).unwrap();
        assert_eq!(
            drain(&b),
            vec![Outbound::Frame(
                r#"{"t":"msg","from":"A","data":{"sdp":"opaque"}}"#.into()
            )]
        );
        assert!(hub.can_turn("A", 1, "B"));
    }

    #[test]
    fn a_device_cannot_be_mutual_with_itself() {
        let mut hub = Hub::default();
        let a = join(&mut hub, "A", 1);
        allow(&mut hub, "A", 1, &["A"]);
        assert!(drain(&a).is_empty());
        assert_eq!(
            hub.forward("A", 1, "A", json!(1)),
            Err(ErrorCode::NotAllowed)
        );
    }

    #[test]
    fn newest_connection_wins_and_the_old_one_goes_stale() {
        let mut hub = Hub::default();
        let old = join(&mut hub, "A", 1);
        let b = join(&mut hub, "B", 2);
        allow(&mut hub, "A", 1, &["B"]);
        allow(&mut hub, "B", 2, &["A"]);
        drain(&old);
        drain(&b);

        let new = join(&mut hub, "A", 3);
        assert_eq!(
            drain(&old),
            vec![Outbound::Close(
                CLOSE_REPLACED,
                "replaced by a newer connection"
            )]
        );
        assert_eq!(drain(&b), vec![presence("A", false)]);

        // The replaced socket's late frames are ignored, including its exit.
        allow(&mut hub, "A", 1, &["B"]);
        assert_eq!(hub.forward("A", 1, "B", json!(1)), Err(ErrorCode::Offline));
        hub.unregister("A", 1);
        assert_eq!(hub.online(), 2);
        assert!(drain(&b).is_empty());

        allow(&mut hub, "A", 3, &["B"]);
        assert_eq!(drain(&new), vec![presence("B", true)]);
        assert_eq!(drain(&b), vec![presence("A", true)]);
    }

    #[test]
    fn a_receiver_buried_in_presence_frames_is_closed() {
        let mut hub = Hub::default();
        let a = join(&mut hub, "A", 1);
        let _b = join(&mut hub, "B", 2);
        allow(&mut hub, "A", 1, &["B"]);
        // B flaps its allow set; A never drains.
        for _ in 0..(MAX_QUEUED_BYTES + CONTROL_HEADROOM) / 40 {
            allow(&mut hub, "B", 2, &["A"]);
            allow(&mut hub, "B", 2, &[]);
        }
        let frames = drain(&a);
        assert!(frames.contains(&Outbound::Close(CLOSE_RATE_LIMITED, "too far behind")));
        let queued: usize = frames
            .iter()
            .map(|frame| match frame {
                Outbound::Frame(text) => text.len(),
                Outbound::Close(..) => 0,
            })
            .sum();
        assert!(queued <= MAX_QUEUED_BYTES + CONTROL_HEADROOM);
    }

    #[test]
    fn a_slow_receiver_turns_into_offline_for_the_sender() {
        let mut hub = Hub::default();
        let _a = join(&mut hub, "A", 1);
        let b = join(&mut hub, "B", 2);
        allow(&mut hub, "A", 1, &["B"]);
        allow(&mut hub, "B", 2, &["A"]);
        let chunk = "x".repeat(512 * 1024);
        let delivered = (0..8)
            .take_while(|_| hub.forward("A", 1, "B", json!(chunk)).is_ok())
            .count();
        assert_eq!(delivered, 3);
        drain(&b);
        assert!(hub.forward("A", 1, "B", json!(chunk)).is_ok());
    }
}

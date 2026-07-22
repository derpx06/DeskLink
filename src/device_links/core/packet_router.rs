use std::collections::HashSet;

use super::errors::ConnectionError;
use crate::device_links::packet::NetworkPacket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone)]
pub struct PacketRouter {
    incoming: HashSet<String>,
    outgoing: HashSet<String>,
}

impl PacketRouter {
    pub fn new(
        incoming: impl IntoIterator<Item = String>,
        outgoing: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            incoming: incoming.into_iter().collect(),
            outgoing: outgoing.into_iter().collect(),
        }
    }

    pub fn authorize(
        &self,
        packet: &NetworkPacket,
        direction: PacketDirection,
    ) -> Result<(), ConnectionError> {
        let allowed = match direction {
            PacketDirection::Incoming => self.incoming.contains(&packet.packet_type),
            PacketDirection::Outgoing => self.outgoing.contains(&packet.packet_type),
        };
        if allowed {
            Ok(())
        } else {
            Err(ConnectionError::UnsupportedPacket(
                packet.packet_type.clone(),
            ))
        }
    }
}

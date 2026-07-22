use std::sync::{Arc, Mutex};

use futures::channel::mpsc;
use serde_json::Value;

use super::device_session::DeviceConnectionState;
use crate::device_links::device::{DeviceNotification, DeviceView};
use crate::device_links::pairing::PairState;

/// Events emitted by the connection core. The GTK, D-Bus, and CLI layers use
/// this stream instead of polling the daemon's complete device list.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    DeviceChanged {
        device: Box<DeviceView>,
    },
    ConnectionChanged {
        device_id: String,
        state: DeviceConnectionState,
        message: Option<String>,
    },
    PairingChanged {
        device_id: String,
        state: PairState,
    },
    TransferChanged {
        transfer_id: String,
        state: String,
        bytes_done: u64,
        bytes_total: u64,
        can_resume: bool,
        error: Option<String>,
    },
    FeatureStateChanged {
        device_id: String,
        feature: String,
        state: String,
        details: Value,
    },
    NotificationReceived {
        device_id: String,
        notification: DeviceNotification,
    },
    Error {
        scope: String,
        device_id: Option<String>,
        message: String,
        retryable: bool,
    },
}

/// Small fan-out event bus shared by the daemon and its clients.
#[derive(Clone, Debug, Default)]
pub struct EventBus {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<CoreEvent>>>>,
}

impl EventBus {
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<CoreEvent> {
        let (sender, receiver) = mpsc::unbounded();
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(sender);
        }
        receiver
    }

    pub fn publish(&self, event: CoreEvent) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.unbounded_send(event.clone()).is_ok());
        }
    }
}

/// Compatibility alias for code that used the original core event name.
pub type ConnectionEvent = CoreEvent;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_delivers_structured_errors() {
        use futures::StreamExt;

        let bus = EventBus::default();
        let mut receiver = bus.subscribe();

        bus.publish(CoreEvent::Error {
            scope: "transport".to_string(),
            device_id: Some("device-1".to_string()),
            message: "connection lost".to_string(),
            retryable: true,
        });

        let event =
            futures::executor::block_on(receiver.next()).expect("event should be delivered");
        match event {
            CoreEvent::Error {
                scope,
                device_id,
                message,
                retryable,
            } => {
                assert_eq!(scope, "transport");
                assert_eq!(device_id.as_deref(), Some("device-1"));
                assert_eq!(message, "connection lost");
                assert!(retryable);
            }
            _ => panic!("unexpected event"),
        }
    }

    #[test]
    fn disconnected_subscribers_are_removed() {
        let bus = EventBus::default();
        let receiver = bus.subscribe();
        drop(receiver);

        bus.publish(CoreEvent::Error {
            scope: "daemon".to_string(),
            device_id: None,
            message: "stopped".to_string(),
            retryable: false,
        });

        assert!(bus.subscribers.lock().unwrap().is_empty());
    }
}

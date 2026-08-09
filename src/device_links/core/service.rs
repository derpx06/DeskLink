use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::events::{CoreEvent, EventBus};
use crate::device_links::device::DeviceView;

/// Shared, product-neutral state exposed to GTK, D-Bus, and CLI adapters.
///
/// The transport worker receives these same Arcs; adapters never maintain a
/// second device list or event stream of their own.
#[derive(Clone, Debug, Default)]
pub struct CoreService {
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    errors: Arc<Mutex<Vec<String>>>,
    events: EventBus,
}

impl CoreService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn devices_storage(&self) -> Arc<Mutex<HashMap<String, DeviceView>>> {
        Arc::clone(&self.devices)
    }

    pub fn errors_storage(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.errors)
    }

    pub fn snapshot(&self) -> Vec<DeviceView> {
        let mut devices: Vec<_> = self
            .devices
            .lock()
            .map(|devices| devices.values().cloned().collect())
            .unwrap_or_default();
        devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        devices
    }

    pub fn drain_errors(&self) -> Vec<String> {
        self.errors
            .lock()
            .map(|mut errors| errors.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn subscribe(&self) -> futures::channel::mpsc::UnboundedReceiver<CoreEvent> {
        self.events.subscribe()
    }

    pub fn publish(&self, event: CoreEvent) {
        self.events.publish(event);
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::CoreService;
    use crate::device_links::core::events::CoreEvent;
    use futures::StreamExt;

    #[test]
    fn adapters_share_the_same_event_stream() {
        let service = CoreService::new();
        let mut events = service.subscribe();
        service.publish(CoreEvent::Error {
            scope: "test".to_string(),
            device_id: None,
            message: "visible".to_string(),
            retryable: false,
        });
        assert!(futures::executor::block_on(events.next()).is_some());
    }
}

use openssl::ssl::SslStream;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::device_links::device_info::DeviceInfo;
use crate::device_links::pairing::PairingHandler;

pub struct SessionLink {
    stream: Option<Arc<Mutex<SslStream<TcpStream>>>>,
    pub certificate_pem: String,
    pub local_public_der: Vec<u8>,
    pub remote_public_der: Vec<u8>,
    pub info: DeviceInfo,
    closed: AtomicBool,
}

impl SessionLink {
    pub fn new(
        stream: Arc<Mutex<SslStream<TcpStream>>>,
        certificate_pem: String,
        local_public_der: Vec<u8>,
        remote_public_der: Vec<u8>,
        info: DeviceInfo,
    ) -> Self {
        Self {
            stream: Some(stream),
            certificate_pem,
            local_public_der,
            remote_public_der,
            info,
            closed: AtomicBool::new(false),
        }
    }

    pub fn stream(&self) -> Result<&Arc<Mutex<SslStream<TcpStream>>>, String> {
        self.stream
            .as_ref()
            .ok_or_else(|| "Session link has no network stream".to_string())
    }

    pub fn verification_key(&self, timestamp: i64) -> String {
        PairingHandler::verification_key(&self.local_public_der, &self.remote_public_der, timestamp)
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(stream) = &self.stream {
            if let Ok(stream) = stream.lock() {
                let _ = stream.get_ref().shutdown(Shutdown::Both);
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn test_placeholder(device_id: &str) -> Self {
        Self {
            stream: None,
            certificate_pem: String::new(),
            local_public_der: Vec::new(),
            remote_public_der: Vec::new(),
            info: DeviceInfo {
                id: device_id.to_string(),
                name: device_id.to_string(),
                device_type: "phone".to_string(),
                protocol_version: 9,
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

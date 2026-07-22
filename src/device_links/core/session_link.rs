use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use openssl::ssl::SslStream;

use crate::device_links::device_info::DeviceInfo;

/// Authenticated transport for one accepted connection generation.
///
/// Pairing is intentionally shared with `DeviceSession`; replacing a TCP/TLS
/// link must never reset the device's trust state.
#[derive(Clone)]
pub(crate) struct Link {
    pub(crate) stream: Arc<Mutex<SslStream<TcpStream>>>,
    pub(crate) certificate_pem: String,
    pub(crate) local_public_der: Vec<u8>,
    pub(crate) remote_public_der: Vec<u8>,
    pub(crate) info: DeviceInfo,
}

impl Link {
    #[cfg(test)]
    pub(crate) fn test_link(info: DeviceInfo) -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("test link listener should bind");
        let address = listener
            .local_addr()
            .expect("test link listener should have an address");
        let client = std::net::TcpStream::connect(address)
            .expect("test link should connect to its loopback peer");
        let (_peer, _) = listener
            .accept()
            .expect("test link listener should accept its peer");
        let context = openssl::ssl::SslContext::builder(openssl::ssl::SslMethod::tls())
            .expect("test SSL context should build")
            .build();
        let ssl = openssl::ssl::Ssl::new(&context).expect("test SSL session should build");
        let stream =
            openssl::ssl::SslStream::new(ssl, client).expect("test SSL stream should build");
        Self {
            stream: Arc::new(Mutex::new(stream)),
            certificate_pem: String::new(),
            local_public_der: Vec::new(),
            remote_public_der: Vec::new(),
            info,
        }
    }

    pub(crate) fn close(&self) {
        if let Ok(stream) = self.stream.lock() {
            let _ = stream.get_ref().shutdown(std::net::Shutdown::Both);
        }
    }
}

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
        // The manager tests only exercise ownership, generations, and
        // cancellation.  Use a local file descriptor wrapped as a
        // TcpStream so those tests do not need network or loopback access in
        // sandboxed CI.  No TLS handshake or I/O is performed on this link.
        use std::os::fd::{FromRawFd, IntoRawFd};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .expect("test link backing file should open");
        let client = unsafe { std::net::TcpStream::from_raw_fd(file.into_raw_fd()) };
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

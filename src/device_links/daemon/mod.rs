use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use super::config::Config;
use super::device::DeviceView;
use super::device_info::DeviceInfo;
use super::pairing::PairingHandler;
use openssl::ssl::SslStream;
use std::net::TcpStream;

pub use commands::DaemonCommand;
pub use handle::DaemonHandle;

pub mod clipboard;
pub mod commands;
pub mod connector;
pub mod discovery;
pub mod file_transfer;
pub mod handle;
pub mod handshake;
pub mod network;
pub mod packet_handler;
pub mod state;
pub mod validation;

pub(super) struct Link {
    pub(super) stream: Arc<Mutex<SslStream<TcpStream>>>,
    pub(super) certificate_pem: String,
    pub(super) local_public_der: Vec<u8>,
    pub(super) remote_public_der: Vec<u8>,
    pub(super) pairing: PairingHandler,
    pub(super) info: DeviceInfo,
}

impl Link {
    pub(super) fn verification_key(&self) -> String {
        PairingHandler::verification_key(
            &self.local_public_der,
            &self.remote_public_der,
            self.pairing.timestamp(),
        )
    }
}

pub(super) struct DaemonWorker {
    pub(super) config: Arc<Mutex<Config>>,
    pub(super) devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    pub(super) links: Arc<Mutex<HashMap<String, Link>>>,
    pub(super) errors: Arc<Mutex<Vec<String>>>,
    pub(super) command_rx: Receiver<DaemonCommand>,
    pub(super) tcp_port: u16,
}

impl DaemonWorker {
    pub(super) fn new(
        devices: Arc<Mutex<HashMap<String, DeviceView>>>,
        errors: Arc<Mutex<Vec<String>>>,
        command_rx: Receiver<DaemonCommand>,
    ) -> Result<Self, String> {
        let config = Arc::new(Mutex::new(Config::load()?));
        let tcp_listener = network::bind_tcp_listener()?;
        let tcp_port = tcp_listener
            .local_addr()
            .map_err(|err| err.to_string())?
            .port();
        let links = Arc::new(Mutex::new(HashMap::new()));

        {
            let config = Arc::clone(&config);
            let devices = Arc::clone(&devices);
            let links = Arc::clone(&links);
            let errors = Arc::clone(&errors);
            thread::spawn(move || {
                connector::incoming_tcp_loop(tcp_listener, config, devices, links, errors)
            });
        }

        Ok(Self {
            config,
            devices,
            links,
            errors,
            command_rx,
            tcp_port,
        })
    }

    pub(super) fn run(self) -> Result<(), String> {
        self.start_udp_listener()?;
        self.start_broadcaster();
        self.start_clipboard_listener();

        while let Ok(command) = self.command_rx.recv() {
            eprintln!("[Daemon] Received command: {:?}", command);
            self.handle_command(command);
        }
        Ok(())
    }
}

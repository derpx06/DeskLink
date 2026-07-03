use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::handshake::finish_secure_link;
use super::network::ssl_connector;
use super::state::{push_error, upsert_device};
use crate::device_links::config::Config;
use crate::device_links::device::DeviceView;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::NetworkPacket;

pub(super) fn incoming_tcp_loop(
    listener: TcpListener,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    links: Arc<Mutex<HashMap<String, super::Link>>>,
    errors: Arc<Mutex<Vec<String>>>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let config = Arc::clone(&config);
                let devices = Arc::clone(&devices);
                let links = Arc::clone(&links);
                let errors = Arc::clone(&errors);
                thread::spawn(move || {
                    if let Err(error) = accept_incoming_device(stream, config, devices, links) {
                        push_error(&errors, error);
                    }
                });
            }
            Err(error) => push_error(&errors, format!("TCP listener failed: {error}")),
        }
    }
}

fn accept_incoming_device(
    stream: TcpStream,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    links: Arc<Mutex<HashMap<String, super::Link>>>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|err| err.to_string())?);
    let mut line = Vec::new();
    loop {
        line.clear();
        reader
            .read_until(b'\n', &mut line)
            .map_err(|err| err.to_string())?;
        if line.is_empty() {
            return Err("Connection closed".to_string());
        }
        if line.len() == 1 && line[0] == b'\n' {
            continue; // Keep-alive
        }
        break;
    }
    let packet = NetworkPacket::deserialize(&line).map_err(|err| err.to_string())?;
    let info = DeviceInfo::from_identity_packet(&packet)
        .ok_or_else(|| "Invalid identity packet".to_string())?;
    let local_id = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .id;
    if packet
        .get_str("targetDeviceId")
        .is_some_and(|target| target != local_id)
    {
        return Err("Connection targeted another device".to_string());
    }
    upsert_device(
        &devices,
        &info,
        stream
            .peer_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        config
            .lock()
            .map(|config| config.is_trusted(&info.id))
            .unwrap_or(false),
    );
    let connector = ssl_connector(&config)?;
    let ssl_stream = connector
        .connect(&info.id, stream)
        .map_err(|err| err.to_string())?;
    finish_secure_link(info, ssl_stream, config, devices, links)
}

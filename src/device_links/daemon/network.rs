use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslStream, SslVerifyMode};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};

use crate::device_links::config::Config;
use crate::device_links::packet::NetworkPacket;
pub(super) use crate::protocol::desklink_v9::{MAX_TCP_PORT, MIN_TCP_PORT, UDP_PORT};

pub(super) const MAX_PACKET_LINE_SIZE: usize = 32 * 1024 * 1024;

pub(super) fn bind_udp_listener() -> Result<UdpSocket, String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|err| err.to_string())?;
    socket
        .set_reuse_address(true)
        .map_err(|err| err.to_string())?;
    socket.set_broadcast(true).map_err(|err| err.to_string())?;
    socket
        .bind(&SocketAddr::from(([0, 0, 0, 0], UDP_PORT)).into())
        .map_err(|err| format!("Could not bind Device Links UDP port {UDP_PORT}: {err}"))?;
    Ok(socket.into())
}

pub(super) fn bind_tcp_listener() -> Result<TcpListener, String> {
    for port in MIN_TCP_PORT..=MAX_TCP_PORT {
        if let Ok(listener) = TcpListener::bind(("0.0.0.0", port)) {
            return Ok(listener);
        }
    }
    Err(format!(
        "Could not bind any Device Links TCP port in {MIN_TCP_PORT}-{MAX_TCP_PORT}"
    ))
}

pub(crate) fn send_packet(
    stream: &Arc<Mutex<SslStream<TcpStream>>>,
    packet: &NetworkPacket,
) -> Result<(), String> {
    let mut stream = stream
        .lock()
        .map_err(|_| "Stream lock poisoned".to_string())?;
    let _ = stream.get_ref().set_nonblocking(false);
    let result = stream
        .write_all(&packet.serialize_line().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string());
    let _ = stream.get_ref().set_nonblocking(true);
    result
}

pub(super) fn read_ssl_packet(stream: &mut SslStream<TcpStream>) -> Result<NetworkPacket, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("Connection closed".to_string());
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            if line.len() == 1 {
                line.clear();
                continue;
            }
            break;
        }
        if line.len() > MAX_PACKET_LINE_SIZE {
            return Err("Packet is too large".to_string());
        }
    }
    NetworkPacket::deserialize(&line).map_err(|err| err.to_string())
}

pub(super) fn ssl_acceptor(config: &Arc<Mutex<Config>>) -> Result<SslAcceptor, String> {
    let config = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?;
    let mut builder =
        SslAcceptor::mozilla_intermediate(SslMethod::tls()).map_err(|err| err.to_string())?;
    builder
        .set_certificate(config.certificate())
        .map_err(|err| err.to_string())?;
    builder
        .set_private_key(config.key())
        .map_err(|err| err.to_string())?;
    builder.set_verify_callback(
        SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT,
        |preverify_ok, context| {
            preverify_ok || (context.error_depth() == 0 && context.error().as_raw() == 18)
        },
    );
    Ok(builder.build())
}

pub(super) fn ssl_connector(config: &Arc<Mutex<Config>>) -> Result<SslConnector, String> {
    let config = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?;
    let mut builder = SslConnector::builder(SslMethod::tls()).expect("TLS method is available");
    builder
        .set_certificate(config.certificate())
        .map_err(|err| err.to_string())?;
    builder
        .set_private_key(config.key())
        .map_err(|err| err.to_string())?;
    builder.set_verify_callback(SslVerifyMode::PEER, |preverify_ok, context| {
        preverify_ok || (context.error_depth() == 0 && context.error().as_raw() == 18)
    });
    Ok(builder.build())
}

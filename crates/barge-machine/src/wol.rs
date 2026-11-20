//! Wake-on-LAN packet generation and sending.
use crate::{Error, ErrorKind, Machine, Result};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
};

/// Parse colon, hyphen or compact 12-digit MAC addresses.
pub fn parse_mac(value: &str) -> Result<[u8; 6]> {
    let bytes = value.as_bytes();
    let compact = match bytes.len() {
        12 if bytes.iter().all(u8::is_ascii_hexdigit) => value.to_string(),
        17 if matches!(bytes[2], b':' | b'-')
            && [2, 5, 8, 11, 14]
                .iter()
                .all(|index| bytes[*index] == bytes[2])
            && bytes.iter().enumerate().all(|(index, byte)| {
                [2, 5, 8, 11, 14].contains(&index) || byte.is_ascii_hexdigit()
            }) =>
        {
            value
                .chars()
                .filter(|character| *character != ':' && *character != '-')
                .collect()
        }
        14 if bytes[4] == b'.'
            && bytes[9] == b'.'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| [4, 9].contains(&index) || byte.is_ascii_hexdigit()) =>
        {
            value
                .chars()
                .filter(|character| *character != '.')
                .collect()
        }
        _ => return Err(Error::usage(format!("invalid MAC address {value:?}"))),
    };
    let mut mac = [0; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&compact[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::usage(format!("invalid MAC address {value:?}")))?;
    }
    Ok(mac)
}

pub(crate) fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Build the 102-byte Wake-on-LAN magic packet.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xff; 102];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&mac);
    }
    packet
}

/// Minimal injectable UDP sender; tests can inspect traffic without a network
/// send.
pub trait DatagramSender {
    /// Send one datagram to `destination`, reporting how many bytes went out.
    fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize>;
}

/// The real sender: one UDP socket with broadcast enabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct UdpSender;

impl DatagramSender for UdpSender {
    fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        socket.send_to(bytes, destination)
    }
}

/// Send the magic packet for `machine` and report the address it went to.
pub fn wake(machine: &Machine, sender: &impl DatagramSender) -> Result<(SocketAddr, usize)> {
    let mac = machine.mac().ok_or_else(|| {
        Error::refused(format!(
            "machine {:?} has no MAC address recorded yet; pair it first",
            machine.name()
        ))
    })?;
    let destination = SocketAddr::from((machine.broadcast(), machine.port()));
    let sent = sender
        .send_to(&magic_packet(parse_mac(mac)?), destination)
        .map_err(|e| {
            Error::new(
                ErrorKind::Io,
                format!("cannot send Wake-on-LAN packet to {destination}: {e}"),
            )
        })?;
    if sent != 102 {
        return Err(Error::new(
            ErrorKind::Io,
            format!("sent {sent} of 102 Wake-on-LAN bytes to {destination}"),
        ));
    }
    Ok((destination, sent))
}

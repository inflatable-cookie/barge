//! mDNS (`_barged._tcp`) service discovery for `barged` on the local network.
//!
//! Clients cannot rely on system `.local` resolution on every platform (for
//! example SteamOS), so this module sends DNS-SD queries directly over UDP to
//! `224.0.0.251:5353` with the unicast-response (QU) bit set and parses the
//! returned PTR, SRV, TXT and A/AAAA records.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{Error, ErrorKind, Machine, Result};

/// The DNS-SD service type `barged` advertises in LAN mode.
pub const MDNS_SERVICE_TYPE: &str = "_barged._tcp.local.";
/// Default browse duration for `barge machine discover`.
pub const DEFAULT_DISCOVER_TIMEOUT: Duration = Duration::from_millis(1500);

const MDNS_MULTICAST: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353));
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const CLASS_IN_QU: u16 = 0x8001;

/// One `barged` instance found on the local network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredMachine {
    /// The daemon's stable machine ID from the `id` TXT record.
    pub id: String,
    /// The daemon's display name from the `name` TXT record (or SRV instance).
    pub name: String,
    /// The daemon's mode (`"lan"` or `"tailscale"`) from the `mode` TXT record.
    pub mode: String,
    /// The daemon's protocol version from the `version` TXT record.
    pub protocol_version: String,
    /// The resolved IP address to dial.
    pub address: String,
    /// The daemon's TCP port from the SRV record.
    pub port: u16,
}

/// Browses `_barged._tcp` on the LAN; tests replace this with a fake.
pub trait MdnsBrowser {
    /// Discover `barged` instances within `timeout`.
    fn browse(&self, timeout: Duration) -> Result<Vec<DiscoveredMachine>>;
}

/// Real mDNS browser sending QU queries over UDP.
#[derive(Debug, Default, Clone, Copy)]
pub struct UdpMdnsBrowser;

impl MdnsBrowser for UdpMdnsBrowser {
    fn browse(&self, timeout: Duration) -> Result<Vec<DiscoveredMachine>> {
        browse_at(MDNS_MULTICAST, timeout)
    }
}

/// Find which registered [`Machine`] corresponds to a discovered instance.
///
/// Matches by `machine_id` first, then falls back to `(hostname, daemon_port)`
/// for records registered manually before discovery.
pub fn match_registered<'a>(
    found: &DiscoveredMachine,
    registered: &'a [Machine],
) -> Option<&'a Machine> {
    if let Some(by_id) = registered
        .iter()
        .find(|machine| machine.id() == Some(found.id.as_str()))
    {
        return Some(by_id);
    }
    registered
        .iter()
        .find(|machine| machine.hostname() == found.address && machine.daemon_port() == found.port)
}

/// Browse mDNS responders via `target` (multicast `224.0.0.251:5353` in
/// production, or a loopback socket in tests).
pub fn browse_at(target: SocketAddr, timeout: Duration) -> Result<Vec<DiscoveredMachine>> {
    if timeout.is_zero() {
        return Ok(Vec::new());
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|e| Error::new(ErrorKind::Io, format!("cannot bind mDNS socket: {e}")))?;
    let query = encode_query(&[(MDNS_SERVICE_TYPE, TYPE_PTR)]);
    socket
        .send_to(&query, target)
        .map_err(|e| Error::new(ErrorKind::Io, format!("cannot send mDNS query: {e}")))?;

    let mut collector = Collector::default();
    let mut queried_instances = HashSet::new();
    let mut queried_hosts = HashSet::new();
    let deadline = Instant::now() + timeout;
    let mut buffer = [0_u8; 4096];

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        let poll = remaining.min(Duration::from_millis(200));
        socket
            .set_read_timeout(Some(poll))
            .map_err(|e| Error::new(ErrorKind::Io, format!("cannot configure mDNS socket: {e}")))?;
        match socket.recv_from(&mut buffer) {
            Ok((len, peer)) => {
                collector.ingest(&buffer[..len], peer.ip());
                let follow_ups =
                    collector.missing_queries(&mut queried_instances, &mut queried_hosts);
                if !follow_ups.is_empty() {
                    let refs: Vec<(&str, u16)> =
                        follow_ups.iter().map(|(n, t)| (n.as_str(), *t)).collect();
                    let _ = socket.send_to(&encode_query(&refs), target);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("mDNS receive failed: {e}"),
                ));
            }
        }
    }

    Ok(collector.finish())
}

#[derive(Default)]
struct ServiceParts {
    port: Option<u16>,
    target: Option<String>,
    txt: BTreeMap<String, String>,
    source_ip: Option<IpAddr>,
}

#[derive(Default)]
struct Collector {
    instances: BTreeMap<String, ServiceParts>,
    hosts: BTreeMap<String, Vec<IpAddr>>,
}

impl Collector {
    fn ingest(&mut self, packet: &[u8], peer_ip: IpAddr) {
        let Ok(records) = parse_records(packet) else {
            return;
        };
        for record in records {
            let name_lower = record.name.to_ascii_lowercase();
            match record.data {
                RecordData::Ptr(instance)
                    if name_lower
                        == MDNS_SERVICE_TYPE.trim_end_matches('.').to_ascii_lowercase() =>
                {
                    let entry = self.instances.entry(instance).or_default();
                    entry.source_ip.get_or_insert(peer_ip);
                }
                RecordData::Srv { port, target } if is_barged_instance(&name_lower) => {
                    let entry = self.instances.entry(record.name).or_default();
                    entry.port = Some(port);
                    entry.target = Some(target);
                    entry.source_ip.get_or_insert(peer_ip);
                }
                RecordData::Txt(pairs) if is_barged_instance(&name_lower) => {
                    let entry = self.instances.entry(record.name).or_default();
                    for (k, v) in pairs {
                        entry.txt.insert(k, v);
                    }
                    entry.source_ip.get_or_insert(peer_ip);
                }
                RecordData::Addr(ip) => {
                    self.hosts.entry(name_lower).or_default().push(ip);
                }
                _ => {}
            }
        }
    }

    fn missing_queries(
        &self,
        queried_instances: &mut HashSet<String>,
        queried_hosts: &mut HashSet<String>,
    ) -> Vec<(String, u16)> {
        let mut out = Vec::new();
        for (instance, parts) in &self.instances {
            if (parts.port.is_none() || parts.txt.is_empty())
                && queried_instances.insert(instance.clone())
            {
                out.push((instance.clone(), TYPE_SRV));
                out.push((instance.clone(), TYPE_TXT));
            }
            if let Some(target) = &parts.target {
                let target_lower = target.to_ascii_lowercase();
                if !self.hosts.contains_key(&target_lower) && queried_hosts.insert(target_lower) {
                    out.push((target.clone(), TYPE_A));
                }
            }
        }
        out
    }

    fn finish(self) -> Vec<DiscoveredMachine> {
        let mut by_id: BTreeMap<String, DiscoveredMachine> = BTreeMap::new();
        for (instance, parts) in self.instances {
            let Some(port) = parts.port else {
                continue;
            };
            let Some(id) = parts.txt.get("id").filter(|s| !s.is_empty()).cloned() else {
                continue;
            };
            let address = parts
                .target
                .as_deref()
                .and_then(|t| self.hosts.get(&t.to_ascii_lowercase()))
                .and_then(|ips| {
                    ips.iter()
                        .find(|ip| ip.is_ipv4())
                        .or_else(|| ips.first())
                        .copied()
                })
                .or(parts.source_ip);
            let Some(address) = address else {
                continue;
            };
            let fallback_name = instance
                .split('.')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("barged")
                .to_string();
            let name = parts
                .txt
                .get("name")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or(fallback_name);
            let mode = parts
                .txt
                .get("mode")
                .cloned()
                .unwrap_or_else(|| "lan".to_string());
            let protocol_version = parts
                .txt
                .get("version")
                .cloned()
                .unwrap_or_else(|| "1".to_string());

            by_id.insert(
                id.clone(),
                DiscoveredMachine {
                    id,
                    name,
                    mode,
                    protocol_version,
                    address: address.to_string(),
                    port,
                },
            );
        }
        let mut list: Vec<DiscoveredMachine> = by_id.into_values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        list
    }
}

fn is_barged_instance(name_lower: &str) -> bool {
    name_lower.ends_with("._barged._tcp.local")
}

/// Encode a DNS query packet for `questions` (`(name, qtype)`).
pub fn encode_query(questions: &[(&str, u16)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&0_u16.to_be_bytes()); // ID
    buf.extend_from_slice(&0_u16.to_be_bytes()); // Flags (standard query)
    buf.extend_from_slice(&(questions.len() as u16).to_be_bytes()); // QDCOUNT
    buf.extend_from_slice(&0_u16.to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0_u16.to_be_bytes()); // NSCOUNT
    buf.extend_from_slice(&0_u16.to_be_bytes()); // ARCOUNT
    for (name, qtype) in questions {
        encode_name(name, &mut buf);
        buf.extend_from_slice(&qtype.to_be_bytes());
        buf.extend_from_slice(&CLASS_IN_QU.to_be_bytes());
    }
    buf
}

fn encode_name(name: &str, out: &mut Vec<u8>) {
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() {
            continue;
        }
        let bytes = label.as_bytes();
        let len = bytes.len().min(63);
        out.push(len as u8);
        out.extend_from_slice(&bytes[..len]);
    }
    out.push(0);
}

#[derive(Debug)]
struct Record {
    name: String,
    data: RecordData,
}

#[derive(Debug)]
enum RecordData {
    Ptr(String),
    Srv { port: u16, target: String },
    Txt(Vec<(String, String)>),
    Addr(IpAddr),
    Other,
}

fn parse_records(packet: &[u8]) -> std::result::Result<Vec<Record>, ()> {
    if packet.len() < 12 {
        return Err(());
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if (flags & 0x8000) == 0 {
        // Ignore queries; only parse responses.
        return Ok(Vec::new());
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let ancount = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let nscount = u16::from_be_bytes([packet[8], packet[9]]) as usize;
    let arcount = u16::from_be_bytes([packet[10], packet[11]]) as usize;

    let mut pos = 12;
    for _ in 0..qdcount {
        let (_, next) = read_name(packet, pos)?;
        pos = next.checked_add(4).ok_or(())?;
        if pos > packet.len() {
            return Err(());
        }
    }

    let total = ancount
        .checked_add(nscount)
        .and_then(|v| v.checked_add(arcount))
        .ok_or(())?;
    let mut records = Vec::new();
    for _ in 0..total {
        if pos >= packet.len() {
            break;
        }
        let (name, next) = read_name(packet, pos)?;
        if next + 10 > packet.len() {
            return Err(());
        }
        let rtype = u16::from_be_bytes([packet[next], packet[next + 1]]);
        let rdlength = u16::from_be_bytes([packet[next + 8], packet[next + 9]]) as usize;
        let rdata_start = next + 10;
        let rdata_end = rdata_start.checked_add(rdlength).ok_or(())?;
        if rdata_end > packet.len() {
            return Err(());
        }
        let data = match rtype {
            TYPE_PTR => {
                let (target, _) = read_name(packet, rdata_start)?;
                RecordData::Ptr(target)
            }
            TYPE_SRV if rdlength >= 6 => {
                let port = u16::from_be_bytes([packet[rdata_start + 4], packet[rdata_start + 5]]);
                let (target, _) = read_name(packet, rdata_start + 6)?;
                RecordData::Srv { port, target }
            }
            TYPE_TXT => RecordData::Txt(parse_txt(&packet[rdata_start..rdata_end])),
            TYPE_A if rdlength == 4 => RecordData::Addr(IpAddr::V4(Ipv4Addr::new(
                packet[rdata_start],
                packet[rdata_start + 1],
                packet[rdata_start + 2],
                packet[rdata_start + 3],
            ))),
            TYPE_AAAA if rdlength == 16 => {
                let mut octets = [0_u8; 16];
                octets.copy_from_slice(&packet[rdata_start..rdata_end]);
                RecordData::Addr(IpAddr::V6(Ipv6Addr::from(octets)))
            }
            _ => RecordData::Other,
        };
        records.push(Record { name, data });
        pos = rdata_end;
    }
    Ok(records)
}

fn read_name(packet: &[u8], start: usize) -> std::result::Result<(String, usize), ()> {
    let mut labels = Vec::new();
    let mut pos = start;
    let mut jumped = false;
    let mut end_pos = start;
    let mut steps = 0;

    loop {
        if pos >= packet.len() || steps > 64 {
            return Err(());
        }
        steps += 1;
        let len = packet[pos];
        if len == 0 {
            if !jumped {
                end_pos = pos + 1;
            }
            break;
        }
        if (len & 0xC0) == 0xC0 {
            if pos + 1 >= packet.len() {
                return Err(());
            }
            let ptr = (u16::from(len & 0x3F) << 8) | u16::from(packet[pos + 1]);
            if !jumped {
                end_pos = pos + 2;
                jumped = true;
            }
            pos = ptr as usize;
            continue;
        }
        if (len & 0xC0) != 0 {
            return Err(());
        }
        let label_len = len as usize;
        let label_start = pos + 1;
        let label_end = label_start.checked_add(label_len).ok_or(())?;
        if label_end > packet.len() {
            return Err(());
        }
        labels.push(String::from_utf8_lossy(&packet[label_start..label_end]).into_owned());
        pos = label_end;
        if !jumped {
            end_pos = pos;
        }
    }
    Ok((labels.join("."), end_pos))
}

fn parse_txt(rdata: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < rdata.len() {
        let len = rdata[pos] as usize;
        pos += 1;
        if pos + len > rdata.len() {
            break;
        }
        let entry = String::from_utf8_lossy(&rdata[pos..pos + len]);
        if let Some((key, value)) = entry.split_once('=') {
            out.push((key.to_string(), value.to_string()));
        }
        pos += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_txt_rdata(entries: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in entries {
            out.push(entry.len() as u8);
            out.extend_from_slice(entry.as_bytes());
        }
        out
    }

    fn encode_response(
        instance: &str,
        host: &str,
        ip: Ipv4Addr,
        port: u16,
        txt: &[&str],
    ) -> Vec<u8> {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&0_u16.to_be_bytes());
        pkt.extend_from_slice(&0x8400_u16.to_be_bytes()); // response + authoritative
        pkt.extend_from_slice(&0_u16.to_be_bytes()); // QDCOUNT
        pkt.extend_from_slice(&1_u16.to_be_bytes()); // ANCOUNT (PTR)
        pkt.extend_from_slice(&0_u16.to_be_bytes()); // NSCOUNT
        pkt.extend_from_slice(&3_u16.to_be_bytes()); // ARCOUNT (SRV, TXT, A)

        // PTR answer
        encode_name(MDNS_SERVICE_TYPE, &mut pkt);
        pkt.extend_from_slice(&TYPE_PTR.to_be_bytes());
        pkt.extend_from_slice(&1_u16.to_be_bytes());
        pkt.extend_from_slice(&120_u32.to_be_bytes());
        let mut ptr_rdata = Vec::new();
        encode_name(instance, &mut ptr_rdata);
        pkt.extend_from_slice(&(ptr_rdata.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&ptr_rdata);

        // SRV additional
        encode_name(instance, &mut pkt);
        pkt.extend_from_slice(&TYPE_SRV.to_be_bytes());
        pkt.extend_from_slice(&1_u16.to_be_bytes());
        pkt.extend_from_slice(&120_u32.to_be_bytes());
        let mut srv_rdata = Vec::new();
        srv_rdata.extend_from_slice(&0_u16.to_be_bytes());
        srv_rdata.extend_from_slice(&0_u16.to_be_bytes());
        srv_rdata.extend_from_slice(&port.to_be_bytes());
        encode_name(host, &mut srv_rdata);
        pkt.extend_from_slice(&(srv_rdata.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&srv_rdata);

        // TXT additional
        encode_name(instance, &mut pkt);
        pkt.extend_from_slice(&TYPE_TXT.to_be_bytes());
        pkt.extend_from_slice(&1_u16.to_be_bytes());
        pkt.extend_from_slice(&120_u32.to_be_bytes());
        let txt_rdata = encode_txt_rdata(txt);
        pkt.extend_from_slice(&(txt_rdata.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&txt_rdata);

        // A additional
        encode_name(host, &mut pkt);
        pkt.extend_from_slice(&TYPE_A.to_be_bytes());
        pkt.extend_from_slice(&1_u16.to_be_bytes());
        pkt.extend_from_slice(&120_u32.to_be_bytes());
        pkt.extend_from_slice(&4_u16.to_be_bytes());
        pkt.extend_from_slice(&ip.octets());

        pkt
    }

    #[test]
    fn loopback_udp_mdns_browse_parses_ptr_srv_txt_and_a() {
        let responder = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let addr = responder.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut buf = [0_u8; 512];
            let (_, peer) = responder.recv_from(&mut buf).unwrap();
            let reply = encode_response(
                "Bazzite._barged._tcp.local.",
                "bazzite.local.",
                Ipv4Addr::new(192, 168, 1, 20),
                9777,
                &[
                    "id=0123456789abcdef",
                    "name=Bazzite",
                    "mode=lan",
                    "version=1",
                ],
            );
            responder.send_to(&reply, peer).unwrap();
        });

        let found = browse_at(addr, Duration::from_millis(250)).unwrap();
        handle.join().unwrap();
        assert_eq!(
            found,
            vec![DiscoveredMachine {
                id: "0123456789abcdef".to_string(),
                name: "Bazzite".to_string(),
                mode: "lan".to_string(),
                protocol_version: "1".to_string(),
                address: "192.168.1.20".to_string(),
                port: 9777,
            }]
        );
    }

    #[test]
    fn compression_pointer_cycle_is_rejected_without_hanging() {
        let mut pkt = vec![0; 14];
        pkt[2] = 0x84; // response
        pkt[7] = 1; // 1 answer
        pkt[12] = 0xC0;
        pkt[13] = 12; // points to itself
        assert!(parse_records(&pkt).is_err());
    }

    #[test]
    fn match_registered_prefers_machine_id_then_address_and_port() {
        let found = DiscoveredMachine {
            id: "mid-1".to_string(),
            name: "Bazzite".to_string(),
            mode: "lan".to_string(),
            protocol_version: "1".to_string(),
            address: "192.168.1.50".to_string(),
            port: 9777,
        };
        let by_id = Machine::discovered("mid-1", "pc", "192.168.1.20", 9777).unwrap();
        let manual = Machine::new("manual", "192.168.1.50", "00:11:22:33:44:ff").unwrap();
        assert_eq!(
            match_registered(&found, &[manual.clone(), by_id.clone()]).map(Machine::name),
            Some("pc")
        );
        assert_eq!(
            match_registered(&found, &[manual]).map(Machine::name),
            Some("manual")
        );
    }
}

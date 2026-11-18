//! Who called: Tailscale identifies the peer node, and the config's allowlist
//! decides whether that node may control the machine.
//!
//! The production lookup asks tailscaled's local API (`whois`) over its unix
//! socket. Everything behind [`PeerLookup`] is a seam, so tests never touch a
//! real tailnet.

use std::fmt;
use std::net::SocketAddr;

/// One identified tailnet node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerNode {
    /// Tailscale's stable node ID, for example `n1234ABCD`.
    pub stable_id: String,
    /// The node's MagicDNS name.
    pub name: String,
}

impl PeerNode {
    pub fn new(stable_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            stable_id: stable_id.into(),
            name: name.into(),
        }
    }
}

/// Why the caller could not be identified.
#[derive(Debug)]
pub struct PeerError {
    message: String,
}

impl PeerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PeerError {}

/// Resolves a peer address to the tailnet node behind it.
pub trait PeerLookup: Send + Sync + 'static {
    fn identify(&self, peer: SocketAddr) -> Result<PeerNode, PeerError>;
}

/// The nodes allowed to call, by stable ID or MagicDNS name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: Vec<String>,
}

impl Allowlist {
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| entry.trim().to_lowercase())
                .filter(|entry| !entry.is_empty())
                .collect(),
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn accepts(&self, node: &PeerNode) -> bool {
        let stable_id = node.stable_id.to_lowercase();
        let name = node.name.to_lowercase();
        self.entries
            .iter()
            .any(|entry| entry == &stable_id || entry == &name)
    }
}

/// tailscaled's local API, `whois` for one peer address.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleLookup {
    socket: std::path::PathBuf,
}

#[cfg(unix)]
impl TailscaleLookup {
    pub fn new(socket: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The local API answers with `{"Node": {"StableID": …, "Name": …}}`.
    fn parse_whois(body: &[u8], peer: SocketAddr) -> Result<PeerNode, PeerError> {
        #[derive(serde::Deserialize)]
        struct Whois {
            #[serde(rename = "Node", default)]
            node: Option<WhoisNode>,
        }
        #[derive(serde::Deserialize)]
        struct WhoisNode {
            #[serde(rename = "StableID", default)]
            stable_id: String,
            #[serde(rename = "ID", default)]
            id: String,
            #[serde(rename = "Name", default)]
            name: String,
        }
        let whois: Whois = serde_json::from_slice(body).map_err(|error| {
            PeerError::new(format!(
                "tailscaled's whois answer for {peer} is not JSON: {error}"
            ))
        })?;
        let node = whois
            .node
            .ok_or_else(|| PeerError::new(format!("tailscaled does not know {peer}")))?;
        let stable_id = if node.stable_id.is_empty() {
            node.id
        } else {
            node.stable_id
        };
        if stable_id.is_empty() {
            return Err(PeerError::new(format!(
                "tailscaled's whois answer for {peer} names no node"
            )));
        }
        Ok(PeerNode::new(stable_id, node.name))
    }
}

#[cfg(unix)]
impl PeerLookup for TailscaleLookup {
    fn identify(&self, peer: SocketAddr) -> Result<PeerNode, PeerError> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let mut stream = UnixStream::connect(&self.socket).map_err(|error| {
            PeerError::new(format!(
                "cannot reach tailscaled at {}: {error}",
                self.socket.display()
            ))
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| PeerError::new(format!("cannot set the whois timeout: {error}")))?;
        let request = format!(
            "GET /localapi/v0/whois?addr={peer} HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).map_err(|error| {
            PeerError::new(format!("cannot ask tailscaled who {peer} is: {error}"))
        })?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).map_err(|error| {
            PeerError::new(format!("cannot read tailscaled's whois answer: {error}"))
        })?;
        let (head, body) = split_response(&response);
        if !head.starts_with("HTTP/1.1 200") {
            return Err(PeerError::new(format!(
                "tailscaled did not identify {peer}: {}",
                head.lines().next().unwrap_or("no status")
            )));
        }
        Self::parse_whois(body, peer)
    }
}

/// Split an HTTP response into its status line and headers, and its body.
#[cfg(unix)]
fn split_response(response: &[u8]) -> (String, &[u8]) {
    let boundary = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4);
    match boundary {
        Some(index) => (
            String::from_utf8_lossy(&response[..index]).into_owned(),
            &response[index..],
        ),
        None => (String::from_utf8_lossy(response).into_owned(), &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn the_allowlist_matches_a_stable_id_or_a_magicdns_name() {
        let allowlist = Allowlist::new([
            "n1234ABCD".to_string(),
            "macbook.tailnet.ts.net".to_string(),
        ]);
        assert!(allowlist.accepts(&PeerNode::new("n1234abcd", "other.tailnet.ts.net")));
        assert!(allowlist.accepts(&PeerNode::new("n9", "macbook.tailnet.ts.net")));
        assert!(!allowlist.accepts(&PeerNode::new("n9", "intruder.tailnet.ts.net")));
        assert!(Allowlist::default().entries().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn whois_documents_name_the_node_and_parse_or_fail_cleanly() {
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 3)), 5000);
        let node = TailscaleLookup::parse_whois(
            br#"{"Node":{"ID":"n123","StableID":"n123","Name":"mac.tail.ts.net"},"User":{}}"#,
            peer,
        )
        .unwrap();
        assert_eq!(node, PeerNode::new("n123", "mac.tail.ts.net"));
        let fallback = TailscaleLookup::parse_whois(
            br#"{"Node":{"ID":"n456","Name":"pc.tail.ts.net"}}"#,
            peer,
        )
        .unwrap();
        assert_eq!(fallback.stable_id, "n456");
        assert!(TailscaleLookup::parse_whois(br#"{"Node":null}"#, peer).is_err());
        assert!(TailscaleLookup::parse_whois(b"not json", peer).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_tailscaled_socket_is_reported_as_unavailable() {
        let lookup = TailscaleLookup::new("/nonexistent/barged-test/tailscaled.sock");
        let error = lookup
            .identify(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234))
            .unwrap_err();
        assert!(
            error.message().contains("cannot reach tailscaled"),
            "{error}"
        );
    }
}

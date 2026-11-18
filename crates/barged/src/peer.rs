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
            stable_id: stable_id.into().trim().to_string(),
            name: name.into().trim().trim_end_matches('.').to_string(),
        }
    }
}

/// The comparison form of a node identity: trimmed, lower case, and without
/// the FQDN's terminal dot. tailcfg's `Node.Name` is an FQDN with a dot
/// (`mac.tail1234.ts.net.`); configs and the install guide write it without
/// one, so both sides meet here.
fn canonical(value: &str) -> String {
    value.trim().trim_end_matches('.').to_lowercase()
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
                .map(|entry| canonical(&entry))
                .filter(|entry| !entry.is_empty())
                .collect(),
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn accepts(&self, node: &PeerNode) -> bool {
        let stable_id = canonical(&node.stable_id);
        let name = canonical(&node.name);
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

    /// The local API answers with a `WhoIsResponse`:
    /// `{"Node": {"StableID": "n1234ABCD", "ID": 1234, "Name": "pc.tail.ts.net."}, "User": {…}}`.
    /// `ID` is `tailcfg.NodeID`, an integer; `StableID` is the string a config
    /// lists. Both are optional here so a shape change cannot fail the whole
    /// document, and the numeric ID is only a last resort: it is not stable
    /// across tailscaled restarts, so an allowlist should use `StableID` or the
    /// name.
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
            id: Option<i64>,
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
        let stable_id = if node.stable_id.trim().is_empty() {
            match node.id {
                Some(id) => id.to_string(),
                None => {
                    return Err(PeerError::new(format!(
                        "tailscaled's whois answer for {peer} names no node"
                    )))
                }
            }
        } else {
            node.stable_id
        };
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
        Self::parse_response(&response, peer)
    }
}

#[cfg(unix)]
impl TailscaleLookup {
    /// One whole HTTP/1.1 response: status, headers, then a body framed the
    /// way the headers say.
    ///
    /// The framing matters. LocalAPI writes the indented `WhoIsResponse`
    /// without a Content-Length, so Go's server sends it
    /// `Transfer-Encoding: chunked` as soon as it exceeds its 2 KiB buffer —
    /// even for a `Connection: close` request. Handing the raw bytes after the
    /// header block to serde would see chunk sizes and trailers instead of
    /// JSON, so the chunked, `Content-Length` and close-delimited cases are all
    /// decoded here.
    fn parse_response(response: &[u8], peer: SocketAddr) -> Result<PeerNode, PeerError> {
        let (head, rest) = split_head(response);
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        if status != "200" {
            return Err(PeerError::new(format!(
                "tailscaled did not identify {peer}: {}",
                head.lines().next().unwrap_or("no status")
            )));
        }
        Self::parse_whois(&frame_body(&head, rest)?, peer)
    }
}

/// Split a response at the end of its header block.
#[cfg(unix)]
fn split_head(response: &[u8]) -> (String, &[u8]) {
    match response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
    {
        Some(index) => (
            String::from_utf8_lossy(&response[..index]).into_owned(),
            &response[index..],
        ),
        None => (String::from_utf8_lossy(response).into_owned(), &[]),
    }
}

/// The response body, decoded from its framing.
#[cfg(unix)]
fn frame_body(head: &str, rest: &[u8]) -> Result<Vec<u8>, PeerError> {
    let mut chunked = false;
    let mut length: Option<usize> = None;
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .split(',')
                .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"));
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
    }
    if chunked {
        return dechunk(rest);
    }
    match length {
        Some(length) if rest.len() >= length => Ok(rest[..length].to_vec()),
        Some(length) => Err(PeerError::new(format!(
            "tailscaled promised {length} body bytes and sent {}",
            rest.len()
        ))),
        // No length and no chunking: the body runs to the end of the response,
        // which the `Connection: close` request guarantees.
        None => Ok(rest.to_vec()),
    }
}

/// Decode a chunked body. Trailers after the last chunk are not part of the
/// document and are ignored.
#[cfg(unix)]
fn dechunk(body: &[u8]) -> Result<Vec<u8>, PeerError> {
    let mut decoded = Vec::new();
    let mut rest = body;
    loop {
        let (line, after) = split_at_crlf(rest).ok_or_else(|| {
            PeerError::new("tailscaled's chunked answer ends inside a chunk header")
        })?;
        let size_field = line.split(|byte| *byte == b';').next().unwrap_or(&[]);
        let size_text = String::from_utf8_lossy(size_field);
        let size = usize::from_str_radix(size_text.trim(), 16).map_err(|_| {
            PeerError::new(format!(
                "tailscaled's chunk size {size_text:?} is not hexadecimal"
            ))
        })?;
        if size == 0 {
            return Ok(decoded);
        }
        if after.len() < size || after.len() - size < 2 {
            return Err(PeerError::new("tailscaled's chunked answer is truncated"));
        }
        decoded.extend_from_slice(&after[..size]);
        if &after[size..size + 2] != b"\r\n" {
            return Err(PeerError::new("tailscaled's chunk is not followed by CRLF"));
        }
        rest = &after[size + 2..];
    }
}

#[cfg(unix)]
fn split_at_crlf(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let index = bytes.windows(2).position(|window| window == b"\r\n")?;
    Some((&bytes[..index], &bytes[index + 2..]))
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

    #[test]
    fn a_magicdns_name_matches_with_or_without_its_terminal_dot() {
        // tailcfg's Node.Name is an FQDN with a dot; the config and the install
        // guide write it without one, in either direction.
        let from_config = Allowlist::new([config_name()]);
        assert!(from_config.accepts(&whois_name()));
        let from_whois = Allowlist::new(["macbook.tail1234.ts.net.".to_string()]);
        assert!(from_whois.accepts(&PeerNode::new("n1", "MACBOOK.tail1234.ts.net")));
        assert!(!from_whois.accepts(&PeerNode::new("n1", "other.tail1234.ts.net.")));
        // Stable IDs are compared exactly, modulo case.
        let ids = Allowlist::new(["n1234ABCD".to_string()]);
        assert!(ids.accepts(&PeerNode::new(" n1234abcd ", "whatever.tail.ts.net")));
        assert!(!ids.accepts(&PeerNode::new("n1234ABCE", "whatever.tail.ts.net")));
    }

    fn config_name() -> String {
        "macbook.tail1234.ts.net".to_string()
    }

    fn whois_name() -> PeerNode {
        PeerNode::new("n1234ABCD", "macbook.tail1234.ts.net.")
    }

    #[cfg(unix)]
    #[test]
    fn a_real_whois_document_parses_with_a_numeric_node_id() {
        // tailcfg.Node.ID is an integer (NodeID) and Name is an FQDN with a
        // trailing dot; StableID is the string an allowlist may name.
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 3)), 5000);
        let body = br#"{"Node":{"ID":1234,"StableID":"n1234ABCD","Name":"macbook.tail1234.ts.net.","User":7,"Key":"nodekey:abc"},"User":{"ID":7,"LoginName":"tom@example.com"}}"#;
        let node = TailscaleLookup::parse_whois(body, peer).unwrap();
        assert_eq!(node, whois_name());
        assert_eq!(node.stable_id, "n1234ABCD");

        // A node with no StableID still parses: the numeric ID is the last
        // resort, and never a whole-document failure.
        let without_stable = TailscaleLookup::parse_whois(
            br#"{"Node":{"ID":456,"Name":"pc.tail1234.ts.net."}}"#,
            peer,
        )
        .unwrap();
        assert_eq!(without_stable.stable_id, "456");
        assert_eq!(without_stable.name, "pc.tail1234.ts.net");

        assert!(TailscaleLookup::parse_whois(br#"{"Node":null}"#, peer).is_err());
        assert!(TailscaleLookup::parse_whois(b"not json", peer).is_err());
    }

    #[cfg(unix)]
    fn whois_document() -> &'static [u8] {
        // The shape LocalAPI returns, indented, long enough to be chunked by
        // Go's server in production.
        br#"{
  "Node": {
    "ID": 1234,
    "StableID": "n1234ABCD",
    "Name": "macbook.tail1234.ts.net.",
    "User": 7,
    "Key": "nodekey:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  },
  "User": {
    "ID": 7,
    "LoginName": "tom@example.com",
    "DisplayName": "Tom"
  }
}"#
    }

    #[cfg(unix)]
    fn chunked_answer(body: &[u8]) -> Vec<u8> {
        let mut answer = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let (first, second) = body.split_at(body.len() / 2);
        for (index, part) in [first, second].into_iter().enumerate() {
            // A chunk extension on the first chunk, as a server may send.
            let extension = if index == 0 { ";ext=1" } else { "" };
            answer.extend_from_slice(format!("{:x}{extension}\r\n", part.len()).as_bytes());
            answer.extend_from_slice(part);
            answer.extend_from_slice(b"\r\n");
        }
        // Last chunk, a trailer, then the terminating CRLF.
        answer.extend_from_slice(b"0\r\nX-Trailer: ignored\r\n\r\n");
        answer
    }

    #[cfg(unix)]
    fn content_length_answer(body: &[u8]) -> Vec<u8> {
        let mut answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        answer.extend_from_slice(body);
        answer
    }

    #[cfg(unix)]
    #[test]
    fn chunked_content_length_and_close_delimited_answers_all_frame() {
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 3)), 5000);
        let expected = PeerNode::new("n1234ABCD", "macbook.tail1234.ts.net");
        for answer in [
            chunked_answer(whois_document()),
            content_length_answer(whois_document()),
            // No length, no chunking: the body runs to the end of the answer.
            [
                b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(),
                whois_document(),
            ]
            .concat(),
        ] {
            let node = TailscaleLookup::parse_response(&answer, peer).unwrap();
            assert_eq!(node, expected, "{}", String::from_utf8_lossy(&answer));
        }
        // A non-200 answer is a refusal, not a parse error.
        let error = TailscaleLookup::parse_response(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            peer,
        )
        .unwrap_err();
        assert!(error.message().contains("did not identify"), "{error}");
        // A truncated chunk is reported by its framing.
        let truncated = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nff\r\n{}\r\n";
        let error = TailscaleLookup::parse_response(truncated, peer).unwrap_err();
        assert!(error.message().contains("truncated"), "{error}");
        // A chunk that is not followed by CRLF is a framing fault, not JSON.
        let unbroken = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}X\r\n";
        let error = TailscaleLookup::parse_response(unbroken, peer).unwrap_err();
        assert!(error.message().contains("CRLF"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn the_lookup_reads_a_real_socket_answer_without_tailscale() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture =
            std::env::temp_dir().join(format!("barged-whois-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&fixture).unwrap();
        let socket = fixture.join("tailscaled.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let answers = [
            chunked_answer(whois_document()),
            content_length_answer(whois_document()),
        ];
        let server = std::thread::spawn(move || {
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let read = stream.read(&mut request).unwrap_or(0);
                assert!(
                    String::from_utf8_lossy(&request[..read])
                        .starts_with("GET /localapi/v0/whois?addr="),
                    "{}",
                    String::from_utf8_lossy(&request[..read])
                );
                stream.write_all(&answer).unwrap();
                stream.flush().unwrap();
            }
        });

        let lookup = TailscaleLookup::new(&socket);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 3)), 5000);
        for _ in 0..2 {
            let node = lookup.identify(peer).unwrap();
            assert_eq!(node, PeerNode::new("n1234ABCD", "macbook.tail1234.ts.net"));
        }
        server.join().unwrap();
        std::fs::remove_dir_all(&fixture).unwrap();
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

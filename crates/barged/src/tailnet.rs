//! The tailnet listener of a combined config: this node's own tailnet identity,
//! asked of tailscaled, and the listener that serves on it.
//!
//! The LAN half never waits on Tailscale. The tailnet side is checked from the
//! serving loop: when tailscaled cannot give an address yet, the listener is
//! retried with a backoff capped at a minute and logs once; once it is up, the
//! address is re-read on a slow interval and the listener rebinds if it moved.
//! Everything behind [`TailnetStatus`] is a seam, so tests never need tailscaled.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::TailnetListen;
use crate::peer::local_api_get;
use crate::{Error, Result};

/// This node as tailscaled reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TailnetSelf {
    /// The node's MagicDNS name, without its terminal dot.
    pub dns_name: Option<String>,
    /// The node's tailnet IPv4 address.
    pub ipv4: Option<Ipv4Addr>,
}

/// Asks tailscaled who this node is on the tailnet.
pub trait TailnetStatus: Send + Sync + 'static {
    fn status(&self) -> Result<TailnetSelf>;
}

/// tailscaled's local API, `status` for this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleStatus {
    socket: PathBuf,
}

impl TailscaleStatus {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
}

impl TailnetStatus for TailscaleStatus {
    fn status(&self) -> Result<TailnetSelf> {
        let body = local_api_get(&self.socket, "/localapi/v0/status", "this node's status")
            .map_err(|error| Error::new(error.message()))?;
        parse_status(&body)
    }
}

/// The parts of tailscaled's status document this daemon reads.
#[derive(Deserialize)]
struct StatusDocument {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Self", default)]
    node: Option<StatusNode>,
}

#[derive(Deserialize)]
struct StatusNode {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<String>,
}

/// Read this node's name and IPv4 address from tailscaled's status. A node
/// that is not running has no tailnet address to serve on.
pub fn parse_status(body: &[u8]) -> Result<TailnetSelf> {
    let document: StatusDocument = serde_json::from_slice(body)
        .map_err(|error| Error::new(format!("tailscaled's status is not JSON: {error}")))?;
    if document.backend_state != "Running" {
        return Err(Error::new(format!(
            "tailscale is {}, not Running",
            if document.backend_state.is_empty() {
                "in an unknown state"
            } else {
                document.backend_state.as_str()
            }
        )));
    }
    let node = document
        .node
        .ok_or_else(|| Error::new("tailscaled's status names no node"))?;
    let dns_name = node.dns_name.trim().trim_end_matches('.').to_string();
    Ok(TailnetSelf {
        dns_name: (!dns_name.is_empty()).then_some(dns_name),
        ipv4: node
            .tailscale_ips
            .iter()
            .find_map(|ip| ip.parse::<Ipv4Addr>().ok()),
    })
}

/// How the tailnet side paces its checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// The first retry after tailscaled cannot give an address.
    pub first_retry: Duration,
    /// The longest wait between retries.
    pub cap: Duration,
    /// How often a bound listener re-reads the address.
    pub recheck: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            first_retry: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            recheck: Duration::from_secs(60),
        }
    }
}

/// The next retry delay: the first one, then double the last, up to the cap.
pub fn next_delay(previous: Option<Duration>, timing: Timing) -> Duration {
    match previous {
        None => timing.first_retry,
        Some(delay) => delay.saturating_mul(2).min(timing.cap),
    }
}

/// The address the tailnet listener binds, given the config and what
/// tailscaled reported. An explicit address needs no status at all.
pub fn resolve(
    listen: TailnetListen,
    lan_port: u16,
    status: &Result<TailnetSelf>,
) -> Result<SocketAddr> {
    match (listen, status) {
        (TailnetListen::Addr(addr), _) => Ok(addr),
        (TailnetListen::Auto, Ok(node)) => node
            .ipv4
            .map(|ip| SocketAddr::new(IpAddr::V4(ip), lan_port))
            .ok_or_else(|| Error::new("tailscale gives this node no IPv4 address yet")),
        (TailnetListen::Auto, Err(error)) => Err(Error::new(error.message())),
    }
}

/// The tailnet listener and its pacing. Owned by the daemon, checked from its
/// serving loop, and never shared with another thread.
pub struct TailnetSide {
    listen: TailnetListen,
    lan_port: u16,
    status: Box<dyn TailnetStatus>,
    timing: Timing,
    listener: Option<TcpListener>,
    /// The address asked for; compared with each check's resolution.
    requested: Option<SocketAddr>,
    /// The address the listener is actually bound to.
    bound: Option<SocketAddr>,
    identity: Option<TailnetSelf>,
    backoff: Option<Duration>,
    next_check: Option<Instant>,
    reported_failure: bool,
}

impl std::fmt::Debug for TailnetSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TailnetSide")
            .field("listen", &self.listen)
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

impl TailnetSide {
    pub fn new(
        listen: TailnetListen,
        lan_port: u16,
        status: Box<dyn TailnetStatus>,
        timing: Timing,
    ) -> Self {
        Self {
            listen,
            lan_port,
            status,
            timing,
            listener: None,
            requested: None,
            bound: None,
            identity: None,
            backoff: None,
            next_check: None,
            reported_failure: false,
        }
    }

    /// The listener, once bound.
    pub fn listener(&self) -> Option<&TcpListener> {
        self.listener.as_ref()
    }

    /// The address the listener is bound to, once bound.
    pub fn bound(&self) -> Option<SocketAddr> {
        self.bound
    }

    /// What tailscaled last reported for this node, when it reported.
    pub fn identity(&self) -> Option<&TailnetSelf> {
        self.identity.as_ref()
    }

    /// Check the tailnet side if its time has come: bind, rebind or retry.
    pub fn check(&mut self, now: Instant) {
        if self.next_check.is_some_and(|due| now < due) {
            return;
        }
        let status = self.status.status();
        match resolve(self.listen, self.lan_port, &status) {
            Ok(addr) => {
                if self.requested != Some(addr) {
                    if let Err(error) = self.bind(addr) {
                        self.retry(now, error.to_string());
                        return;
                    }
                }
                self.identity = status.ok();
                self.backoff = None;
                self.reported_failure = false;
                self.next_check = Some(now + self.timing.recheck);
            }
            Err(error) => self.retry(now, error.to_string()),
        }
    }

    /// Bind `addr`, first letting go of the old listener: a rebind on the same
    /// port cannot coexist with it.
    fn bind(&mut self, addr: SocketAddr) -> std::io::Result<()> {
        let previous = self.bound.take();
        self.listener = None;
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let actual = listener.local_addr()?;
        match previous {
            Some(old) => eprintln!(
                "barged: tailnet address changed from {old} to {actual}; serving there now"
            ),
            None => eprintln!("barged: serving on the tailnet at {actual}"),
        }
        self.listener = Some(listener);
        self.requested = Some(addr);
        self.bound = Some(actual);
        Ok(())
    }

    fn retry(&mut self, now: Instant, reason: String) {
        if !self.reported_failure {
            eprintln!("barged: tailnet side not up yet: {reason}; retrying");
            self.reported_failure = true;
        }
        let delay = next_delay(self.backoff, self.timing);
        self.backoff = Some(delay);
        self.next_check = Some(now + delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A status whose answer the test changes between checks.
    #[derive(Clone, Default)]
    struct Scripted(Arc<Mutex<Option<TailnetSelf>>>);

    impl Scripted {
        fn answer(&self, node: Option<TailnetSelf>) {
            *self.0.lock().unwrap() = node;
        }
    }

    impl TailnetStatus for Scripted {
        fn status(&self) -> Result<TailnetSelf> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| Error::new("tailscaled is not running"))
        }
    }

    fn node(ip: [u8; 4]) -> TailnetSelf {
        TailnetSelf {
            dns_name: Some("pc.tail1234.ts.net".to_string()),
            ipv4: Some(Ipv4Addr::from(ip)),
        }
    }

    #[test]
    fn a_running_node_reports_its_magicdns_name_and_ipv4_address() {
        let body = br#"{
            "BackendState": "Running",
            "Self": {"DNSName": "pc.tail1234.ts.net.", "TailscaleIPs": ["fd7a::1", "100.64.0.7"]}
        }"#;
        assert_eq!(parse_status(body).unwrap(), node([100, 64, 0, 7]));

        let stopped = br#"{"BackendState": "Stopped", "Self": {"DNSName": "pc.tail1234.ts.net."}}"#;
        assert!(parse_status(stopped).is_err());
        assert!(parse_status(br#"{"BackendState": "Running"}"#).is_err());
        assert!(parse_status(b"not json").is_err());
    }

    #[test]
    fn the_first_retry_doubles_up_to_the_cap() {
        let timing = Timing {
            first_retry: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            recheck: Duration::from_secs(60),
        };
        let mut delay = None;
        let mut seen = Vec::new();
        for _ in 0..9 {
            let next = next_delay(delay, timing);
            seen.push(next.as_secs());
            delay = Some(next);
        }
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn auto_uses_the_tailnet_ipv4_on_the_lan_port_and_an_explicit_address_needs_no_status() {
        let up: Result<TailnetSelf> = Ok(node([100, 64, 0, 7]));
        assert_eq!(
            resolve(TailnetListen::Auto, 9777, &up).unwrap(),
            "100.64.0.7:9777".parse().unwrap()
        );
        let down: Result<TailnetSelf> = Err(Error::new("tailscaled is not running"));
        assert!(resolve(TailnetListen::Auto, 9777, &down).is_err());
        let fixed = "100.64.0.9:4000".parse().unwrap();
        assert_eq!(
            resolve(TailnetListen::Addr(fixed), 9777, &down).unwrap(),
            fixed
        );
    }

    /// A loopback port that was free a moment ago. Tests bind it, so no test
    /// depends on a port another process might hold.
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn the_listener_waits_for_tailscale_then_binds_and_follows_an_address_change() {
        let status = Scripted::default();
        let timing = Timing {
            first_retry: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            recheck: Duration::from_secs(60),
        };
        let port = free_port();
        let mut side =
            TailnetSide::new(TailnetListen::Auto, port, Box::new(status.clone()), timing);
        let start = Instant::now();

        // Tailscale is not up: nothing is bound, and the next try is a second on.
        side.check(start);
        assert!(side.listener().is_none());
        assert!(side.identity().is_none());

        // Before the retry is due no status is read and nothing changes.
        status.answer(Some(node([127, 0, 0, 1])));
        side.check(start + Duration::from_millis(500));
        assert!(side.listener().is_none());

        // The retry is due: the address is loopback on the LAN port.
        side.check(start + Duration::from_secs(1));
        assert_eq!(
            side.bound(),
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
        );
        assert!(side.listener().is_some());
        assert_eq!(side.identity(), Some(&node([127, 0, 0, 1])));

        // A bound listener re-reads its address only on the recheck interval.
        status.answer(Some(node([0, 0, 0, 0])));
        side.check(start + Duration::from_secs(30));
        assert_eq!(side.bound().unwrap().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

        // On the recheck the new address is seen and the listener moves to it.
        side.check(start + Duration::from_secs(62));
        assert!(side.bound().unwrap().ip().is_unspecified());
        assert_eq!(side.identity(), Some(&node([0, 0, 0, 0])));
    }
}

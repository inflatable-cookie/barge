//! The HTTP surface: power endpoints, pairing, approval and client revocation.
//!
//! Every response is JSON with a `schema` field, errors included. The daemon
//! handles one request at a time on purpose: there is one operator and one
//! pending power action, so a thread pool would add concurrency (and races)
//! without adding anything.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use barge_machine::{
    auth, Cancel, CancelResult, ClientList, ClientRevoked, DaemonError, PairApproved, PairDecision,
    PairDenied, PairRequest, PairStatus, PowerRequest, PowerResult, PowerState, State,
    CANCEL_SCHEMA, PAIR_DECISION_SCHEMA, PAIR_REQUEST_SCHEMA, POWER_REQUEST_SCHEMA,
};

use crate::config::{Serving, TailnetListen};
use crate::mdns::{MdnsAdvertiser, MdnsRegistration, ServiceAdvertisement};
use crate::net::{NetworkLookup, SystemNetworkLookup};
use crate::notify::{PairAction, PairNotification, PairingNotifier};
use crate::pairing::{PairDecisionError, PairStartError, PairingStore};
use crate::state::{DaemonState, LEGACY_CLIENT_ID};
use crate::tailnet::{TailnetSelf, TailnetSide, TailnetStatus, TailscaleStatus, Timing};
use crate::{Allowlist, Clock, Config, Error, PeerLookup, PeerNode, PowerController, RequestError};

/// The largest request body the daemon reads; the documents are a few lines.
const MAX_BODY: u64 = 16 * 1024;
/// How long a blocked accept waits before the stop flag is checked again.
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HEADERS: usize = 16 * 1024;

/// Which listener a request came in on. Pairing and client management are a
/// LAN act, so they are served only on the LAN surface; the tailnet surface
/// authenticates by a paired client's signature or by the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Lan,
    Tailnet,
}

/// One daemon: state, power, mDNS advertisement, pairing and client auth.
pub struct Daemon {
    listen: SocketAddr,
    display_name: String,
    controller: PowerController,
    peers: Arc<dyn PeerLookup>,
    allowlist: Allowlist,
    clock: Arc<dyn Clock>,
    state: Arc<DaemonState>,
    pairing: Arc<PairingStore>,
    network: Arc<dyn NetworkLookup>,
    notifier: Arc<dyn PairingNotifier>,
    advertiser: Arc<dyn MdnsAdvertiser>,
    serving: Serving,
    /// The tailnet listener's address, when the config names one for the
    /// combined shape; the side is built from it.
    tailnet_listen: Option<TailnetListen>,
    tailnet: Option<Mutex<TailnetSide>>,
    nonces: Mutex<HashMap<String, u64>>,
}

impl fmt::Debug for Daemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Daemon")
            .field("listen", &self.listen)
            .field("machine_id", &self.state.machine_id())
            .field("allowlist", &self.allowlist)
            .field("serving", &self.serving)
            .field("pending", &self.controller.pending())
            .finish_non_exhaustive()
    }
}

/// A refusal: the HTTP status and the error document to send.
type Refusal = (u16, String);
type Answer = std::result::Result<(u16, String), Refusal>;

impl Daemon {
    pub fn new(
        config: &Config,
        controller: PowerController,
        peers: Arc<dyn PeerLookup>,
        clock: Arc<dyn Clock>,
    ) -> crate::Result<Self> {
        let state = DaemonState::open(
            config.resolved_state_dir(),
            config.key_path().as_deref(),
            clock.now_unix_ms(),
        )?;
        let tailnet_listen = config.tailscale.as_ref().map(|block| block.listen);
        let mut daemon = Self {
            listen: config.listen,
            display_name: config.display_name(),
            controller,
            peers,
            allowlist: config.allowlist(),
            clock,
            state: Arc::new(state),
            pairing: Arc::new(PairingStore::default()),
            network: Arc::new(SystemNetworkLookup),
            notifier: default_notifier(),
            advertiser: default_advertiser(),
            serving: config.serving(),
            tailnet_listen,
            tailnet: None,
            nonces: Mutex::new(HashMap::new()),
        };
        daemon.tailnet = tailnet_listen.map(|listen| {
            Mutex::new(TailnetSide::new(
                listen,
                config.listen.port(),
                Box::new(TailscaleStatus::new(config.tailnet_socket())),
                Timing::default(),
            ))
        });
        Ok(daemon)
    }

    /// Replace how the tailnet side asks tailscaled about this node, and how it
    /// paces its checks. Tests use this; a daemon in production keeps tailscaled.
    pub fn with_tailnet(mut self, status: Box<dyn TailnetStatus>, timing: Timing) -> Self {
        if let Some(listen) = self.tailnet_listen {
            let lan_port = self.listen.port();
            self.tailnet = Some(Mutex::new(TailnetSide::new(
                listen, lan_port, status, timing,
            )));
        }
        self
    }

    /// The tailnet listener's bound address, once it is up.
    pub fn tailnet_address(&self) -> Option<SocketAddr> {
        self.tailnet.as_ref().and_then(|side| lock(side).bound())
    }

    /// What tailscaled last reported for this node, when the tailnet side is up.
    fn tailnet_identity(&self) -> Option<TailnetSelf> {
        self.tailnet
            .as_ref()
            .and_then(|side| lock(side).identity().cloned())
    }

    /// Whether this daemon has a LAN half: pairing, mDNS and HMAC clients.
    fn has_lan(&self) -> bool {
        matches!(self.serving, Serving::Lan | Serving::Both)
    }

    /// The surface the primary listener serves: the tailnet address in the
    /// tailnet-only shape, the LAN address otherwise.
    fn primary_surface(&self) -> Surface {
        match self.serving {
            Serving::Tailnet => Surface::Tailnet,
            Serving::Lan | Serving::Both => Surface::Lan,
        }
    }

    pub fn with_network(mut self, network: Arc<dyn NetworkLookup>) -> Self {
        self.network = network;
        self
    }

    pub fn with_notifier(mut self, notifier: Arc<dyn PairingNotifier>) -> Self {
        self.notifier = notifier;
        self
    }

    pub fn with_advertiser(mut self, advertiser: Arc<dyn MdnsAdvertiser>) -> Self {
        self.advertiser = advertiser;
        self
    }

    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    pub fn state_store(&self) -> &DaemonState {
        &self.state
    }

    /// Bind the configured address. A wildcard bind is refused by the config,
    /// so the socket is reachable only where it was named.
    pub fn bind(&self) -> crate::Result<TcpListener> {
        let listener = TcpListener::bind(self.listen)
            .map_err(|error| Error::new(format!("cannot bind {}: {error}", self.listen)))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| Error::new(format!("cannot configure listener: {error}")))?;
        Ok(listener)
    }

    /// Bind the configured address and, when serving a specific LAN address,
    /// also bind loopback on the same port so local callers (Decky plugin,
    /// `barged pair --approve`) can reach `127.0.0.1:<port>`. The tailnet
    /// listener is not bound here: it comes up from the serving loop, so a
    /// tailscaled that is not running yet does not stop the LAN half.
    pub fn bind_all(&self) -> crate::Result<Vec<TcpListener>> {
        let primary = self.bind()?;
        let mut listeners = Vec::new();
        if self.has_lan() && !self.listen.ip().is_loopback() && !self.listen.ip().is_unspecified() {
            if let Ok(local_addr) = primary.local_addr() {
                let loopback = match local_addr.ip() {
                    IpAddr::V4(_) => {
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_addr.port())
                    }
                    IpAddr::V6(_) => {
                        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), local_addr.port())
                    }
                };
                match TcpListener::bind(loopback)
                    .and_then(|sock| sock.set_nonblocking(true).map(|()| sock))
                {
                    Ok(sock) => listeners.push(sock),
                    Err(error) => eprintln!(
                        "barged: warning: cannot bind localhost listener {loopback}: {error}"
                    ),
                }
            }
        }
        listeners.insert(0, primary);
        Ok(listeners)
    }

    /// Serve one listener until `running` clears.
    pub fn serve(&self, listener: TcpListener, running: &AtomicBool) -> crate::Result<()> {
        self.serve_all(vec![listener], running)
    }

    /// Serve all listeners until `running` clears, and the tailnet listener
    /// when the config names one. The first listener is the primary address;
    /// the rest (loopback, for local approval UIs) are LAN. Advertises via
    /// mDNS when there is a LAN half.
    pub fn serve_all(
        &self,
        listeners: Vec<TcpListener>,
        running: &AtomicBool,
    ) -> crate::Result<()> {
        self.check_tailnet(Instant::now());
        let _mdns = self.start_mdns(listeners.first());
        let primary = self.primary_surface();
        while running.load(Ordering::SeqCst) {
            let mut accepted_any = false;
            self.check_tailnet(Instant::now());
            for (index, listener) in listeners.iter().enumerate() {
                let surface = if index == 0 { primary } else { Surface::Lan };
                match listener.accept() {
                    Ok((stream, peer)) => {
                        accepted_any = true;
                        self.handle(stream, peer, surface);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => {
                        return Err(Error::new(format!("cannot accept a request: {error}")))
                    }
                }
            }
            if let Some((stream, peer)) = self.accept_tailnet()? {
                accepted_any = true;
                self.handle(stream, peer, Surface::Tailnet);
            }
            if !accepted_any {
                std::thread::sleep(ACCEPT_POLL);
            }
        }
        Ok(())
    }

    /// Let the tailnet side bind, rebind or retry if its time has come.
    fn check_tailnet(&self, now: Instant) {
        if let Some(side) = &self.tailnet {
            lock(side).check(now);
        }
    }

    /// The next tailnet connection, if one is waiting. The side's lock is
    /// released before the request runs: answering it reads the same side.
    fn accept_tailnet(&self) -> crate::Result<Option<(TcpStream, SocketAddr)>> {
        let Some(side) = &self.tailnet else {
            return Ok(None);
        };
        let side = lock(side);
        let Some(listener) = side.listener() else {
            return Ok(None);
        };
        match listener.accept() {
            Ok(pair) => Ok(Some(pair)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(Error::new(format!(
                "cannot accept a tailnet request: {error}"
            ))),
        }
    }

    fn start_mdns(&self, primary: Option<&TcpListener>) -> Option<Box<dyn MdnsRegistration>> {
        if !self.has_lan() {
            return None;
        }
        let port = primary
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or_else(|| self.listen.port());
        let info = self.network.interface_for(self.listen.ip());
        let mut txt = vec![
            ("id".to_string(), self.state.machine_id().to_string()),
            ("name".to_string(), self.display_name.clone()),
            ("mode".to_string(), "lan".to_string()),
            ("version".to_string(), "1".to_string()),
        ];
        if let Some(name) = self.tailnet_identity().and_then(|node| node.dns_name) {
            txt.push(("tailscale".to_string(), name));
        }
        let service = ServiceAdvertisement {
            service_type: "_barged._tcp".to_string(),
            instance_name: self.display_name.clone(),
            port,
            address: self.listen.ip(),
            interface_index: info.interface_index,
            txt,
        };
        match self.advertiser.advertise(&service) {
            Ok(registration) => Some(registration),
            Err(error) => {
                eprintln!("barged: mDNS advertisement unavailable: {error}");
                None
            }
        }
    }

    fn handle(&self, mut stream: TcpStream, peer: SocketAddr, surface: Surface) {
        let result = stream
            .set_nonblocking(false)
            .and_then(|()| stream.set_read_timeout(Some(READ_TIMEOUT)))
            .and_then(|()| stream.set_write_timeout(Some(READ_TIMEOUT)))
            .map_err(|e| (400, failure("bad_request", e.to_string())))
            .and_then(|()| read_request(&mut stream))
            .and_then(|request| self.route(&request, Some(peer), surface));
        let (status, body) = result.unwrap_or_else(|refusal| refusal);
        let response = format!("HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", reason(status), body.len());
        if let Err(error) = stream.write_all(response.as_bytes()) {
            eprintln!("barged: cannot answer the client: {error}");
        }
    }

    fn route(&self, request: &Request, peer: Option<SocketAddr>, surface: Surface) -> Answer {
        let body = &request.body;
        let url = request.url.to_string();
        let path = url.split(['?', '#']).next().unwrap_or("").to_string();
        let method = request.method.as_str();

        if path == "/v1/pair" {
            require_lan_surface(surface)?;
            return match method {
                "POST" => self.pair_start(body),
                _ => Err(method_not_allowed(path, method)),
            };
        }

        if path == "/v1/pair/pending" {
            require_lan_surface(surface)?;
            self.authorize_lan_or_localhost(request, peer)?;
            return match method {
                "GET" => self.pair_pending(),
                _ => Err(method_not_allowed(path, method)),
            };
        }

        if let Some(rest) = path.strip_prefix("/v1/pair/") {
            require_lan_surface(surface)?;
            if let Some(id) = rest.strip_suffix("/approve") {
                if !valid_segment(id) {
                    return Err((
                        404,
                        failure("not_found", format!("{path} is not an endpoint")),
                    ));
                }
                self.authorize_lan_or_localhost(request, peer)?;
                return match method {
                    "POST" => self.pair_approve(id, body),
                    _ => Err(method_not_allowed(path, method)),
                };
            }
            if let Some(id) = rest.strip_suffix("/deny") {
                if !valid_segment(id) {
                    return Err((
                        404,
                        failure("not_found", format!("{path} is not an endpoint")),
                    ));
                }
                self.authorize_lan_or_localhost(request, peer)?;
                return match method {
                    "POST" => self.pair_deny(id, body),
                    _ => Err(method_not_allowed(path, method)),
                };
            }
            if valid_segment(rest) {
                return match method {
                    "GET" => self.pair_status(rest),
                    _ => Err(method_not_allowed(path, method)),
                };
            }
        }

        let caller = self.authorize_caller(request, peer, surface)?;

        if path == "/v1/clients" {
            require_lan_surface(surface)?;
            return match method {
                "GET" => self.list_clients(),
                _ => Err(method_not_allowed(path, method)),
            };
        }

        if let Some(client_id) = path.strip_prefix("/v1/clients/") {
            if valid_segment(client_id) {
                require_lan_surface(surface)?;
                return match method {
                    "DELETE" => self.revoke_client(client_id),
                    _ => Err(method_not_allowed(path, method)),
                };
            }
        }

        match (method, path.as_str()) {
            ("GET", "/v1/state") => self.state_answer(),
            ("POST", "/v1/power") => self.power(body, &caller),
            ("POST", "/v1/power/cancel") => self.cancel(body),
            (_, known @ ("/v1/state" | "/v1/power" | "/v1/power/cancel")) => {
                Err(method_not_allowed(known.to_string(), method))
            }
            (_, other) => Err((
                404,
                failure("not_found", format!("{other} is not an endpoint")),
            )),
        }
    }

    /// Who may call, by the surface the request arrived on. On the LAN a
    /// signed request is a paired client. On the tailnet a paired client's
    /// signature counts too (when this daemon has a LAN half to pair on), and
    /// anything else must be on the allowlist, as it always was.
    fn authorize_caller(
        &self,
        request: &Request,
        peer: Option<SocketAddr>,
        surface: Surface,
    ) -> std::result::Result<PeerNode, Refusal> {
        match surface {
            Surface::Lan => self.authorize_lan(request, request.body.as_bytes()),
            Surface::Tailnet if self.has_lan() => self
                .authorize_lan(request, request.body.as_bytes())
                .or_else(|_| self.authorize(peer)),
            Surface::Tailnet => self.authorize(peer),
        }
    }

    fn authorize_lan_or_localhost(
        &self,
        request: &Request,
        peer: Option<SocketAddr>,
    ) -> std::result::Result<PeerNode, Refusal> {
        if peer.is_some_and(|addr| addr.ip().is_loopback()) && !has_auth_headers(request) {
            return Ok(PeerNode::new("localhost", "localhost"));
        }
        self.authorize_lan(request, request.body.as_bytes())
    }

    /// Tailscale identifies the caller; the config decides whether it may call.
    fn authorize(&self, peer: Option<SocketAddr>) -> std::result::Result<PeerNode, Refusal> {
        let Some(peer) = peer else {
            return Err((
                403,
                failure("not_authorized", "the request carries no peer address"),
            ));
        };
        let node = self.peers.identify(peer).map_err(|error| {
            (
                403,
                failure(
                    "not_authorized",
                    format!("cannot identify the caller at {peer}: {error}"),
                ),
            )
        })?;
        if !self.allowlist.accepts(&node) {
            return Err((
                403,
                failure(
                    "not_authorized",
                    format!(
                        "node {:?} ({}) is not on the allowlist",
                        node.name, node.stable_id
                    ),
                ),
            ));
        }
        Ok(node)
    }

    fn authorize_lan(
        &self,
        request: &Request,
        body: &[u8],
    ) -> std::result::Result<PeerNode, Refusal> {
        let refused = || (401, failure("not_authorized", "authentication failed"));
        let client_id = header_value(request, auth::CLIENT_ID).unwrap_or(LEGACY_CLIENT_ID);
        let client = self
            .state
            .find_client(client_id)
            .map_err(|error| {
                eprintln!("barged: cannot read paired clients: {error}");
                (500, failure("internal", "cannot read paired clients"))
            })?
            .ok_or_else(refused)?;
        let key = client.key_bytes().ok_or_else(|| {
            eprintln!("barged: stored key for client {:?} is invalid", client.id);
            (500, failure("internal", "invalid stored client key"))
        })?;

        let timestamp = header_value(request, auth::TIMESTAMP).ok_or_else(refused)?;
        let nonce = header_value(request, auth::NONCE).ok_or_else(refused)?;
        let signature = header_value(request, auth::SIGNATURE).ok_or_else(refused)?;
        let seconds: u64 = timestamp.parse().map_err(|_| refused())?;
        let now = self.clock.now_unix_ms() / 1000;
        if seconds.abs_diff(now) > 60
            || auth::decode(nonce, 16).is_none()
            || !auth::verify(
                &key,
                request.method.as_str(),
                &request.url,
                timestamp,
                nonce,
                body,
                signature,
            )
        {
            return Err(refused());
        }
        let replay_key = format!("{}:{nonce}", client.id);
        let mut nonces = self.nonces.lock().map_err(|_| refused())?;
        nonces.retain(|_, expiry| *expiry >= now);
        if nonces.contains_key(&replay_key) || nonces.len() >= 10000 {
            return Err(refused());
        }
        nonces.insert(replay_key, seconds.saturating_add(60));
        Ok(PeerNode::new(client.id, client.name))
    }

    fn state_answer(&self) -> Answer {
        let info = self.network.interface_for(self.listen.ip());
        let tailnet = self.tailnet_identity();
        let state = State::new(PowerState::Awake, self.clock.now_unix_ms())
            .with_pending(self.controller.pending())
            .with_machine_id(self.state.machine_id().to_string())
            .with_mac(info.mac)
            .with_broadcast(info.broadcast)
            .with_tailscale(
                tailnet.as_ref().and_then(|node| node.dns_name.clone()),
                tailnet.and_then(|node| node.ipv4),
            );
        Ok((200, encode(&state)?))
    }

    fn power(&self, body: &str, caller: &PeerNode) -> Answer {
        let document: PowerRequest = self.document(body)?;
        if document.schema != POWER_REQUEST_SCHEMA {
            return Err(schema_mismatch(POWER_REQUEST_SCHEMA, &document.schema));
        }
        match self.controller.request(document.action, document.delay_s) {
            Ok(pending) => {
                eprintln!(
                    "barged: {} accepted from {} ({}), fires at {}",
                    pending.action.as_str(),
                    caller.name,
                    caller.stable_id,
                    pending.deadline_unix_ms
                );
                Ok((
                    202,
                    encode(&PowerResult::new(pending.action, pending.deadline_unix_ms))?,
                ))
            }
            Err(error @ RequestError::Pending(_)) => {
                Err((409, failure("pending", error.to_string())))
            }
            Err(error @ RequestError::DelayTooLong { .. }) => {
                Err((400, failure("bad_request", error.to_string())))
            }
        }
    }

    fn cancel(&self, body: &str) -> Answer {
        let document: Cancel = self.document(body)?;
        if document.schema != CANCEL_SCHEMA {
            return Err(schema_mismatch(CANCEL_SCHEMA, &document.schema));
        }
        let cancelled = self.controller.cancel();
        Ok((200, encode(&CancelResult::new(cancelled))?))
    }

    fn pair_start(&self, body: &str) -> Answer {
        let document: PairRequest = self.document(body)?;
        if document.schema != PAIR_REQUEST_SCHEMA {
            return Err(schema_mismatch(PAIR_REQUEST_SCHEMA, &document.schema));
        }
        let now_ms = self.clock.now_unix_ms();
        let (response, code) = self
            .pairing
            .start(&document.client_name, &document.public_key, now_ms)
            .map_err(|error| match error {
                PairStartError::InvalidInput(msg) => (400, failure("bad_request", msg)),
                PairStartError::TooManyPending => (
                    409,
                    failure("too_many_pending", "too many pending pairing requests"),
                ),
                PairStartError::Internal(msg) => (500, failure("internal", msg)),
            })?;

        let notice = PairNotification {
            request_id: response.request_id.clone(),
            client_name: document.client_name.trim().to_string(),
            code,
        };
        let pairing = Arc::clone(&self.pairing);
        let state = Arc::clone(&self.state);
        let clock = Arc::clone(&self.clock);
        let callback = Arc::new(move |request_id: &str, action: PairAction| {
            let now = clock.now_unix_ms();
            match action {
                PairAction::Approve => {
                    if let Err(err) = pairing.approve(request_id, &state, now) {
                        eprintln!("barged: notification approve failed: {err:?}");
                    }
                }
                PairAction::Deny => {
                    if let Err(err) = pairing.deny(request_id, now) {
                        eprintln!("barged: notification deny failed: {err:?}");
                    }
                }
            }
        });
        self.notifier.notify(notice, callback);

        Ok((202, encode(&response)?))
    }

    fn pair_status(&self, request_id: &str) -> Answer {
        let now_ms = self.clock.now_unix_ms();
        let status = self
            .pairing
            .status(request_id, now_ms)
            .ok_or_else(|| (404, failure("not_found", "unknown pairing request")))?;
        Ok((200, encode(&status)?))
    }

    fn pair_pending(&self) -> Answer {
        let now_ms = self.clock.now_unix_ms();
        let requests = self.pairing.pending(now_ms);
        Ok((200, encode(&barge_machine::PendingPairList::new(requests))?))
    }

    fn pair_approve(&self, request_id: &str, body: &str) -> Answer {
        self.check_optional_decision_body(body)?;
        let now_ms = self.clock.now_unix_ms();
        let client = self
            .pairing
            .approve(request_id, &self.state, now_ms)
            .map_err(decision_refusal)?;
        Ok((200, encode(&PairApproved::new(request_id, client))?))
    }

    fn pair_deny(&self, request_id: &str, body: &str) -> Answer {
        self.check_optional_decision_body(body)?;
        let now_ms = self.clock.now_unix_ms();
        self.pairing
            .deny(request_id, now_ms)
            .map_err(decision_refusal)?;
        Ok((200, encode(&PairDenied::new(request_id))?))
    }

    fn check_optional_decision_body(&self, body: &str) -> std::result::Result<(), Refusal> {
        if body.trim().is_empty() {
            return Ok(());
        }
        let document: PairDecision = self.document(body)?;
        if document.schema != PAIR_DECISION_SCHEMA {
            return Err(schema_mismatch(PAIR_DECISION_SCHEMA, &document.schema));
        }
        Ok(())
    }

    fn list_clients(&self) -> Answer {
        let clients = self
            .state
            .list_clients()
            .map_err(|e| (500, failure("internal", e.to_string())))?
            .into_iter()
            .map(|c| c.summary())
            .collect();
        Ok((200, encode(&ClientList::new(clients))?))
    }

    fn revoke_client(&self, client_id: &str) -> Answer {
        let removed = self
            .state
            .remove_client(client_id)
            .map_err(|e| (500, failure("internal", e.to_string())))?;
        if !removed {
            return Err((
                404,
                failure("not_found", format!("client {client_id:?} is not paired")),
            ));
        }
        Ok((200, encode(&ClientRevoked::new(client_id))?))
    }

    fn document<T: serde::de::DeserializeOwned>(
        &self,
        body: &str,
    ) -> std::result::Result<T, Refusal> {
        if body.trim().is_empty() {
            return Err((
                400,
                failure("bad_request", "the request body must be a JSON document"),
            ));
        }
        serde_json::from_str(body).map_err(|error| {
            (
                400,
                failure(
                    "bad_request",
                    format!("the request body is not a valid document: {error}"),
                ),
            )
        })
    }
}

/// Pairing and client management are a LAN act: physical presence at the LAN
/// address is what counts. The tailnet listener refuses them outright.
fn require_lan_surface(surface: Surface) -> std::result::Result<(), Refusal> {
    match surface {
        Surface::Lan => Ok(()),
        Surface::Tailnet => Err((
            400,
            failure(
                "bad_request",
                "pairing and client management are served only on the LAN address",
            ),
        )),
    }
}

/// Lock a side the daemon owns. A panic elsewhere cannot leave the side in a
/// state this code cannot use, so a poisoned lock is read through.
fn lock<T>(side: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    side.lock().unwrap_or_else(PoisonError::into_inner)
}

fn decision_refusal(error: PairDecisionError) -> Refusal {
    match error {
        PairDecisionError::NotFound => (404, failure("not_found", "unknown pairing request")),
        PairDecisionError::AlreadyDecided(PairStatus::Approved) => (
            409,
            failure("already_decided", "pairing request was already approved"),
        ),
        PairDecisionError::AlreadyDecided(_) => (
            409,
            failure("already_decided", "pairing request was already denied"),
        ),
        PairDecisionError::Expired => (409, failure("expired", "pairing request has expired")),
        PairDecisionError::Internal(msg) => (500, failure("internal", msg)),
    }
}

fn method_not_allowed(path: String, method: &str) -> Refusal {
    (
        405,
        failure(
            "method_not_allowed",
            format!("{path} does not accept {method}"),
        ),
    )
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn header_value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn has_auth_headers(request: &Request) -> bool {
    header_value(request, auth::SIGNATURE).is_some()
        || header_value(request, auth::TIMESTAMP).is_some()
        || header_value(request, auth::NONCE).is_some()
        || header_value(request, auth::CLIENT_ID).is_some()
}

#[cfg(target_os = "linux")]
fn default_notifier() -> Arc<dyn PairingNotifier> {
    Arc::new(crate::notify::DesktopNotifier)
}

#[cfg(not(target_os = "linux"))]
fn default_notifier() -> Arc<dyn PairingNotifier> {
    Arc::new(crate::notify::NoopNotifier)
}

#[cfg(target_os = "linux")]
fn default_advertiser() -> Arc<dyn MdnsAdvertiser> {
    Arc::new(crate::mdns::AvahiAdvertiser)
}

#[cfg(not(target_os = "linux"))]
fn default_advertiser() -> Arc<dyn MdnsAdvertiser> {
    Arc::new(crate::mdns::UnavailableMdns)
}

struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> std::result::Result<Request, Refusal> {
    let bad = || {
        (
            400,
            failure("bad_request", "invalid or incomplete HTTP request"),
        )
    };
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= MAX_HEADERS {
            return Err(bad());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(bad)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| bad())?;
        let n = stream.read(&mut chunk).map_err(|_| bad())?;
        if n == 0 {
            return Err(bad());
        }
        bytes.extend_from_slice(&chunk[..n]);
    };
    if header_end > MAX_HEADERS {
        return Err(bad());
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(
        parsed.parse(&bytes[..header_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err(bad());
    }
    let method = parsed.method.ok_or_else(bad)?.to_string();
    let url = parsed.path.ok_or_else(bad)?.to_string();
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            std::str::from_utf8(h.value)
                .map(|v| (h.name.to_string(), v.to_string()))
                .map_err(|_| bad())
        })
        .collect::<std::result::Result<_, _>>()?;
    if headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
    {
        return Err(bad());
    }
    let lengths: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .collect();
    if lengths.len() > 1 {
        return Err(bad());
    }
    let length = match lengths.first() {
        Some((_, v)) => v.trim().parse::<usize>().map_err(|_| bad())?,
        None => 0,
    };
    if length as u64 > MAX_BODY {
        return Err((
            413,
            failure(
                "too_large",
                format!("the request body is larger than {MAX_BODY} bytes"),
            ),
        ));
    }
    while bytes.len() - header_end < length {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(bad)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| bad())?;
        let n = stream.read(&mut chunk).map_err(|_| bad())?;
        if n == 0 {
            return Err(bad());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let body = std::str::from_utf8(&bytes[header_end..header_end + length])
        .map_err(|_| bad())?
        .to_string();
    Ok(Request {
        method,
        url,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    }
}

fn schema_mismatch(expected: &str, actual: &str) -> Refusal {
    (
        400,
        failure(
            "bad_request",
            format!("expected schema {expected}, not {actual:?}"),
        ),
    )
}

fn encode<T: serde::Serialize>(document: &T) -> std::result::Result<String, Refusal> {
    serde_json::to_string(document).map_err(|error| {
        (
            500,
            failure("internal", format!("cannot encode a response: {error}")),
        )
    })
}

/// An error document. The literal fallback keeps a refused response a document
/// with a schema even if encoding fails.
fn failure(kind: &str, message: impl Into<String>) -> String {
    serde_json::to_string(&DaemonError::new(kind, message)).unwrap_or_else(|_| {
        String::from(
            r#"{"schema":"barge.daemon-error/1","error":"internal","message":"cannot encode the error"}"#,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PeerError, Result as DaemonResult};

    #[derive(Debug)]
    struct FixedLookup(Option<PeerNode>);

    impl PeerLookup for FixedLookup {
        fn identify(&self, _peer: SocketAddr) -> std::result::Result<PeerNode, PeerError> {
            self.0
                .clone()
                .ok_or_else(|| PeerError::new("no tailnet identity for this peer"))
        }
    }

    #[derive(Debug)]
    struct NoPower;

    impl crate::PowerBackend for NoPower {
        fn perform(&self, action: barge_machine::PowerActionKind) -> DaemonResult<()> {
            Err(Error::new(format!(
                "no power in this test ({})",
                action.as_str()
            )))
        }
    }

    fn tailscale_config(temp: &barge::testing::TempDir) -> Config {
        Config::parse(
            format!(
                r#"{{"listen":"127.0.0.1:0","allowed_nodes":["mac"],"state_dir":{}}}"#,
                serde_json::to_string(temp.path()).unwrap()
            )
            .as_bytes(),
        )
        .unwrap()
    }

    fn lan_config(temp: &barge::testing::TempDir) -> Config {
        Config::parse(
            format!(
                r#"{{"listen":"127.0.0.1:0","mode":"lan","state_dir":{}}}"#,
                serde_json::to_string(temp.path()).unwrap()
            )
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn slow_drip_cannot_prevent_cancel() {
        let temp = barge::testing::TempDir::new("barged-drip").unwrap();
        let config = tailscale_config(&temp);
        let controller = PowerController::new(Arc::new(NoPower), Arc::new(crate::SystemClock));
        controller
            .request(barge_machine::PowerActionKind::Shutdown, 60)
            .unwrap();
        let daemon = Arc::new(
            Daemon::new(
                &config,
                controller,
                Arc::new(FixedLookup(Some(PeerNode::new("id", "mac")))),
                Arc::new(crate::SystemClock),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, ready) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            accepted.send(()).unwrap();
            daemon.handle(stream, peer, Surface::Tailnet);
            let (stream, peer) = listener.accept().unwrap();
            daemon.handle(stream, peer, Surface::Tailnet);
        });
        let mut slow = TcpStream::connect(address).unwrap();
        slow.write_all(b"POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{")
            .unwrap();
        ready.recv().unwrap();
        let (stop_drip, stopped) = std::sync::mpsc::channel();
        let drip = std::thread::spawn(move || {
            while stopped.recv_timeout(Duration::from_millis(500)).is_err() {
                // Keep the connection open even if the server has timed it out.
                let _ = slow.write_all(b" ");
            }
        });
        let start = Instant::now();
        let result = barge_machine::DaemonClient::at(format!("http://{address}")).cancel();
        let elapsed = start.elapsed();
        stop_drip.send(()).unwrap();
        drip.join().unwrap();
        server.join().unwrap();
        assert!(result.unwrap().cancelled);
        assert!(elapsed < Duration::from_secs(5));
    }

    #[test]
    fn an_unidentifiable_caller_is_refused_before_any_endpoint_runs() {
        let temp = barge::testing::TempDir::new("barged-unid").unwrap();
        let config = tailscale_config(&temp);
        let controller = PowerController::new(Arc::new(NoPower), Arc::new(crate::SystemClock));
        let daemon = Daemon::new(
            &config,
            controller,
            Arc::new(FixedLookup(None)),
            Arc::new(crate::SystemClock),
        )
        .unwrap();
        let refusal = daemon
            .authorize(Some("127.0.0.1:5000".parse().unwrap()))
            .unwrap_err();
        assert_eq!(refusal.0, 403);
        assert!(refusal.1.contains("not_authorized"));
        assert_eq!(
            daemon.authorize(None).unwrap_err().0,
            403,
            "no peer address is a refusal too"
        );
    }

    #[test]
    fn an_identified_node_off_the_allowlist_is_refused() {
        let temp = barge::testing::TempDir::new("barged-allow").unwrap();
        let config = Config::parse(
            format!(
                r#"{{"listen":"127.0.0.1:0","allowed_nodes":["mac.tailnet.ts.net"],"state_dir":{}}}"#,
                serde_json::to_string(temp.path()).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
        let controller = PowerController::new(Arc::new(NoPower), Arc::new(crate::SystemClock));
        let daemon = Daemon::new(
            &config,
            controller,
            Arc::new(FixedLookup(Some(PeerNode::new(
                "n9",
                "intruder.tailnet.ts.net",
            )))),
            Arc::new(crate::SystemClock),
        )
        .unwrap();
        let refusal = daemon
            .authorize(Some("127.0.0.1:5000".parse().unwrap()))
            .unwrap_err();
        assert_eq!(refusal.0, 403);
        assert!(
            refusal.1.contains("is not on the allowlist"),
            "{}",
            refusal.1
        );
    }

    #[test]
    fn approval_routes_refuse_unsigned_non_localhost_caller_and_allow_localhost() {
        let temp = barge::testing::TempDir::new("barged-approval-auth").unwrap();
        let config = lan_config(&temp);
        let controller = PowerController::new(Arc::new(NoPower), Arc::new(crate::SystemClock));
        let daemon = Daemon::new(
            &config,
            controller,
            Arc::new(FixedLookup(None)),
            Arc::new(crate::SystemClock),
        )
        .unwrap();

        let non_localhost: SocketAddr = "192.0.2.50:40000".parse().unwrap();
        let localhost: SocketAddr = "127.0.0.1:40000".parse().unwrap();

        for (method, url) in [
            ("GET", "/v1/pair/pending"),
            ("POST", "/v1/pair/req1/approve"),
            ("POST", "/v1/pair/req1/deny"),
        ] {
            let req = Request {
                method: method.to_string(),
                url: url.to_string(),
                headers: Vec::new(),
                body: String::new(),
            };
            let refused = daemon
                .route(&req, Some(non_localhost), Surface::Lan)
                .unwrap_err();
            assert_eq!(refused.0, 401, "{method} {url}");
            let no_peer = daemon.route(&req, None, Surface::Lan).unwrap_err();
            assert_eq!(no_peer.0, 401, "{method} {url} with None peer");
        }

        // Localhost succeeds on pending list (empty) and reaches 404 on unknown req1.
        let pending_req = Request {
            method: "GET".to_string(),
            url: "/v1/pair/pending".to_string(),
            headers: Vec::new(),
            body: String::new(),
        };
        assert_eq!(
            daemon
                .route(&pending_req, Some(localhost), Surface::Lan)
                .unwrap()
                .0,
            200
        );

        let approve_req = Request {
            method: "POST".to_string(),
            url: "/v1/pair/req1/approve".to_string(),
            headers: Vec::new(),
            body: String::new(),
        };
        assert_eq!(
            daemon
                .route(&approve_req, Some(localhost), Surface::Lan)
                .unwrap_err()
                .0,
            404
        );

        // Unsigned localhost is STILL refused on protected non-approval routes.
        let state_req = Request {
            method: "GET".to_string(),
            url: "/v1/state".to_string(),
            headers: Vec::new(),
            body: String::new(),
        };
        assert_eq!(
            daemon
                .route(&state_req, Some(localhost), Surface::Lan)
                .unwrap_err()
                .0,
            401
        );
    }
}

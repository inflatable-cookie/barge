//! The daemon over real loopback HTTP.
//!
//! Each test binds `127.0.0.1:0`, so nothing touches a tailnet, a real D-Bus or
//! logind. The caller's identity, power backend, network interface, mDNS
//! advertiser and desktop notifier are fakes; the client is the one `barge`
//! ships, so the pair is exercised together.

// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use barge_machine::{
    auth, parse_public_key, DaemonClient, KeyPair, PairRole, PairStatus, Pending, PowerActionKind,
    PowerState, State, CANCEL_RESULT_SCHEMA, DAEMON_ERROR_SCHEMA, POWER_RESULT_SCHEMA,
    STATE_SCHEMA,
};
use barged::{
    ActionCallback, Allowlist, Clock, Config, Daemon, InterfaceInfo, MdnsAdvertiser,
    MdnsRegistration, NetworkLookup, PairAction, PairNotification, PairingNotifier, PeerError,
    PeerLookup, PeerNode, PowerBackend, PowerController, Result, ServiceAdvertisement, TailnetSelf,
    TailnetStatus, Timing,
};

#[derive(Debug, Default)]
struct RecordingBackend(Mutex<Vec<PowerActionKind>>);

impl PowerBackend for RecordingBackend {
    fn perform(&self, action: PowerActionKind) -> Result<()> {
        self.0.lock().unwrap().push(action);
        Ok(())
    }
}

impl RecordingBackend {
    fn called(&self) -> Vec<PowerActionKind> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Debug, Default)]
struct FakeClock(Mutex<u64>);

impl FakeClock {
    fn at(&self, millis: u64) {
        *self.0.lock().unwrap() = millis;
    }
    fn advance(&self, millis: u64) {
        *self.0.lock().unwrap() += millis;
    }
    fn now(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}

impl Clock for FakeClock {
    fn now_unix_ms(&self) -> u64 {
        self.now()
    }
}

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
struct FakeNetwork(InterfaceInfo);

impl NetworkLookup for FakeNetwork {
    fn interface_for(&self, _ip: IpAddr) -> InterfaceInfo {
        self.0.clone()
    }
}

#[derive(Debug, Default)]
struct FakeMdns {
    advertised: Mutex<Vec<ServiceAdvertisement>>,
    dropped: Arc<AtomicBool>,
    fail: bool,
}

struct FakeRegistration(Arc<AtomicBool>);

impl MdnsRegistration for FakeRegistration {}

impl Drop for FakeRegistration {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl MdnsAdvertiser for FakeMdns {
    fn advertise(&self, service: &ServiceAdvertisement) -> Result<Box<dyn MdnsRegistration>> {
        self.advertised.lock().unwrap().push(service.clone());
        if self.fail {
            return Err(barged::Error::new("Avahi not running"));
        }
        Ok(Box::new(FakeRegistration(Arc::clone(&self.dropped))))
    }
}

#[derive(Default)]
struct FakeNotifier {
    notices: Mutex<Vec<(PairNotification, ActionCallback)>>,
}

impl PairingNotifier for FakeNotifier {
    fn notify(&self, notice: PairNotification, on_action: ActionCallback) {
        self.notices.lock().unwrap().push((notice, on_action));
    }
}

/// A tailnet status the test controls: `None` is tailscaled not running.
#[derive(Clone, Default)]
struct FakeTailnet(Arc<Mutex<Option<TailnetSelf>>>);

impl FakeTailnet {
    fn up(&self, node: TailnetSelf) {
        *self.0.lock().unwrap() = Some(node);
    }
}

impl TailnetStatus for FakeTailnet {
    fn status(&self) -> Result<TailnetSelf> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| barged::Error::new("tailscaled is not running"))
    }
}

/// This node as the tailnet names it.
fn pc_node() -> TailnetSelf {
    TailnetSelf {
        dns_name: Some("pc.tail1234.ts.net".to_string()),
        ipv4: Some(Ipv4Addr::new(100, 64, 0, 7)),
    }
}

/// Retries at millisecond pace, so a test sees the tailnet side come up.
fn quick_timing() -> Timing {
    Timing {
        first_retry: Duration::from_millis(10),
        cap: Duration::from_millis(40),
        recheck: Duration::from_millis(40),
    }
}

/// A daemon on loopback, its client, its fakes and its stop flag.
struct Harness {
    _temp: barge::testing::TempDir,
    daemon: Arc<Daemon>,
    client: DaemonClient,
    address: SocketAddr,
    controller: PowerController,
    backend: Arc<RecordingBackend>,
    clock: Arc<FakeClock>,
    allowlist: Allowlist,
    mdns: Arc<FakeMdns>,
    notifier: Arc<FakeNotifier>,
    machine_id: String,
    running: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Harness {
    fn with_identity(allowlist: &[&str], identity: Option<PeerNode>) -> Self {
        let temp = barge::testing::TempDir::new("barged-harness").unwrap();
        let nodes = allowlist
            .iter()
            .map(|node| format!("{node:?}"))
            .collect::<Vec<_>>()
            .join(",");
        let config = Config::parse(
            format!(
                r#"{{"listen":"127.0.0.1:0","allowed_nodes":[{nodes}],"state_dir":{}}}"#,
                serde_json::to_string(temp.path()).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
        Self::start(temp, config, identity, false, None)
    }

    fn lan(temp: barge::testing::TempDir, config: Config, fail_mdns: bool) -> Self {
        Self::start(temp, config, None, fail_mdns, None)
    }

    /// A combined daemon: LAN on loopback, and a tailnet listener on loopback
    /// that the `tailnet` fake reports. `tailnet_nodes` is the tailnet
    /// allowlist, written as JSON strings.
    fn combined(identity: Option<PeerNode>, tailnet_nodes: &[&str], tailnet: FakeTailnet) -> Self {
        let temp = barge::testing::TempDir::new("barged-combined").unwrap();
        let nodes = tailnet_nodes
            .iter()
            .map(|node| format!("{node:?}"))
            .collect::<Vec<_>>()
            .join(",");
        let config = Config::parse(
            format!(
                r#"{{"listen":"127.0.0.1:0","mode":"lan","name":"Linux PC","state_dir":{},"tailscale":{{"listen":"127.0.0.1:0","allowed_nodes":[{nodes}]}}}}"#,
                serde_json::to_string(temp.path()).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
        Self::start(temp, config, identity, false, Some(tailnet))
    }

    /// Wait until the tailnet listener is up, and return its address.
    fn wait_for_tailnet(&self) -> SocketAddr {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(address) = self.daemon.tailnet_address() {
                return address;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the tailnet listener never came up"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn start(
        temp: barge::testing::TempDir,
        config: Config,
        identity: Option<PeerNode>,
        fail_mdns: bool,
        tailnet: Option<FakeTailnet>,
    ) -> Self {
        let backend = Arc::new(RecordingBackend::default());
        let clock = Arc::new(FakeClock::default());
        clock.at(1_000_000);
        let performer: Arc<dyn PowerBackend> = backend.clone();
        let controller = PowerController::new(performer, clock.clone());
        let peers: Arc<dyn PeerLookup> = Arc::new(FixedLookup(identity));
        let mdns = Arc::new(FakeMdns {
            fail: fail_mdns,
            ..Default::default()
        });
        let notifier = Arc::new(FakeNotifier::default());
        let network = Arc::new(FakeNetwork(InterfaceInfo {
            interface_found: true,
            broadcast_capable: true,
            mac: Some("aa:bb:cc:dd:ee:ff".to_string()),
            broadcast: Some(Ipv4Addr::new(192, 0, 2, 255)),
            interface_index: Some(2),
            lookup_error: None,
        }));
        let mut built = Daemon::new(&config, controller.clone(), peers, clock.clone())
            .unwrap()
            .with_network(network)
            .with_mdns_and_notifier(mdns.clone(), notifier.clone());
        if let Some(tailnet) = tailnet {
            built = built.with_tailnet(Box::new(tailnet), quick_timing());
        }
        let daemon = Arc::new(built);
        let machine_id = daemon.state_store().machine_id().to_string();
        let listeners = daemon.bind_all().unwrap();
        let address = listeners[0].local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let serving = Arc::clone(&running);
        let serving_daemon = Arc::clone(&daemon);
        let handle = thread::spawn(move || {
            let served = serving_daemon.serve_all(listeners, &serving);
            assert!(served.is_ok(), "{served:?}");
        });
        Self {
            _temp: temp,
            daemon,
            client: DaemonClient::at(format!("http://{address}")),
            address,
            controller,
            backend,
            clock,
            allowlist: config.allowlist(),
            mdns,
            notifier,
            machine_id,
            running,
            server: Some(handle),
        }
    }

    fn new() -> Self {
        Self::with_identity(
            &["macbook.tailnet.ts.net"],
            Some(PeerNode::new("n1", "macbook.tailnet.ts.net")),
        )
    }

    fn state(&self) -> State {
        self.client.state().unwrap()
    }

    /// One raw request, for the cases the typed client cannot express.
    fn raw(&self, request: &str) -> (u16, String) {
        raw_at(self.address, request)
    }
}

/// One raw request to `address`, answered as (status, body).
fn raw_at(address: SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap();
    (status, body.to_string())
}

trait DaemonTestExt {
    fn with_mdns_and_notifier(self, mdns: Arc<FakeMdns>, notifier: Arc<FakeNotifier>) -> Self;
}

impl DaemonTestExt for Daemon {
    fn with_mdns_and_notifier(self, mdns: Arc<FakeMdns>, notifier: Arc<FakeNotifier>) -> Self {
        self.with_advertiser(mdns).with_notifier(notifier)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

#[test]
fn the_state_endpoint_reports_awake_machine_id_mac_broadcast_and_pending() {
    let harness = Harness::new();
    let state = harness.state();
    assert_eq!(state.schema, STATE_SCHEMA);
    assert_eq!(state.power, PowerState::Awake);
    assert_eq!(state.observed_at_unix_ms, harness.clock.now());
    assert_eq!(state.machine_id, harness.machine_id);
    assert_eq!(state.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    assert_eq!(state.broadcast, Some(Ipv4Addr::new(192, 0, 2, 255)));
    assert_eq!(state.pending, None);
    assert!(harness
        .allowlist
        .accepts(&PeerNode::new("n1", "macbook.tailnet.ts.net")));
    // Tailscale mode never advertises over mDNS.
    assert!(harness.mdns.advertised.lock().unwrap().is_empty());

    let result = harness.client.power(PowerActionKind::Sleep, 15).unwrap();
    assert_eq!(result.schema, POWER_RESULT_SCHEMA);
    assert_eq!(result.action, PowerActionKind::Sleep);
    assert_eq!(result.deadline_unix_ms, harness.clock.now() + 15_000);

    let state = harness.state();
    assert_eq!(
        state.power,
        PowerState::Awake,
        "still awake while counting down"
    );
    assert_eq!(
        state.pending,
        Some(Pending {
            action: PowerActionKind::Sleep,
            deadline_unix_ms: result.deadline_unix_ms,
        })
    );
}

#[test]
fn a_countdown_fires_the_backend_once_when_its_deadline_passes() {
    let harness = Harness::new();
    harness.client.power(PowerActionKind::Sleep, 15).unwrap();
    harness.clock.advance(14_999);
    assert_eq!(harness.controller.fire_due(), None);
    assert!(harness.backend.called().is_empty());

    harness.clock.advance(1);
    assert_eq!(harness.controller.fire_due(), Some(PowerActionKind::Sleep));
    assert_eq!(harness.backend.called(), vec![PowerActionKind::Sleep]);
    assert_eq!(harness.state().pending, None);
    // Nothing fires twice.
    assert_eq!(harness.controller.fire_due(), None);
    assert_eq!(harness.backend.called(), vec![PowerActionKind::Sleep]);
}

#[test]
fn cancel_before_the_deadline_stops_the_action_and_reports_whether_it_did() {
    let harness = Harness::new();
    harness.client.power(PowerActionKind::Restart, 60).unwrap();
    let cancelled = harness.client.cancel().unwrap();
    assert_eq!(cancelled.schema, CANCEL_RESULT_SCHEMA);
    assert!(cancelled.cancelled);
    harness.clock.advance(120_000);
    assert_eq!(harness.controller.fire_due(), None);
    assert!(harness.backend.called().is_empty());
    assert_eq!(harness.state().pending, None);

    let nothing = harness.client.cancel().unwrap();
    assert!(!nothing.cancelled);
}

#[test]
fn a_second_action_is_refused_while_one_is_pending() {
    let harness = Harness::new();
    harness.client.power(PowerActionKind::Sleep, 15).unwrap();
    let error = harness
        .client
        .power(PowerActionKind::Shutdown, 15)
        .unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(error.message().contains("already pending"), "{error}");
    assert_eq!(
        harness.state().pending.map(|pending| pending.action),
        Some(PowerActionKind::Sleep)
    );
    assert!(harness.backend.called().is_empty());
}

#[test]
fn a_caller_not_on_the_allowlist_is_refused_and_changes_nothing() {
    let harness = Harness::with_identity(
        &["macbook.tailnet.ts.net"],
        Some(PeerNode::new("n9", "intruder.tailnet.ts.net")),
    );
    let error = harness.client.state().unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(
        error.message().contains("is not on the allowlist"),
        "{error}"
    );
    let error = harness
        .client
        .power(PowerActionKind::Shutdown, 0)
        .unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(harness.backend.called().is_empty());

    let (status, body) =
        harness.raw("GET /v1/state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(status, 403);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
}

#[test]
fn an_unidentifiable_caller_is_refused() {
    let harness = Harness::with_identity(&["n1"], None);
    let error = harness.client.state().unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(
        error.message().contains("cannot identify the caller"),
        "{error}"
    );
}

#[test]
fn the_backend_is_called_with_only_the_three_allowed_actions() {
    let harness = Harness::new();
    for action in [
        PowerActionKind::Sleep,
        PowerActionKind::Restart,
        PowerActionKind::Shutdown,
    ] {
        harness.client.power(action, 0).unwrap();
        assert_eq!(harness.controller.fire_due(), Some(action));
    }
    assert_eq!(
        harness.backend.called(),
        vec![
            PowerActionKind::Sleep,
            PowerActionKind::Restart,
            PowerActionKind::Shutdown
        ]
    );

    // A request body cannot name a method: an action outside the enum, a wrong
    // schema and an over-long delay are all refused before the backend.
    for body in [
        r#"{"schema":"barge.power-request/1","action":"format-disk","delay_s":0}"#,
        r#"{"schema":"barge.power-request/1","action":"sleep","delay_s":100000}"#,
        r#"{"schema":"barge.power-request/2","action":"sleep","delay_s":0}"#,
    ] {
        let request = format!(
            "POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, answer) = harness.raw(&request);
        assert_eq!(status, 400, "{body}");
        assert!(answer.contains(DAEMON_ERROR_SCHEMA), "{answer}");
    }
    assert_eq!(
        harness.backend.called(),
        vec![
            PowerActionKind::Sleep,
            PowerActionKind::Restart,
            PowerActionKind::Shutdown
        ]
    );
}

#[test]
fn unknown_paths_and_methods_answer_with_an_error_document() {
    let harness = Harness::new();
    let (status, body) =
        harness.raw("GET /v1/nothing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(status, 404);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
    let (status, body) = harness.raw(
        "POST /v1/state HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 405);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
}

#[test]
fn a_document_that_is_not_json_is_a_bad_request() {
    let harness = Harness::new();
    let (status, body) = harness.raw(
        "POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\nConnection: close\r\n\r\nnope!!\n",
    );
    assert_eq!(status, 400);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
    assert!(harness.backend.called().is_empty());
}

#[test]
fn an_absent_daemon_is_unreachable_and_never_read_as_asleep() {
    // Bind and drop a listener: the port is closed but was, briefly, real.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let error = DaemonClient::at(format!("http://{address}"))
        .state()
        .unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Unreachable);
    assert!(error.message().contains("is unreachable"), "{error}");
    assert!(!error.message().contains("asleep"), "{error}");
}

#[test]
fn lan_pairing_approve_deny_expiry_per_client_auth_and_revoke() {
    let temp = barge::testing::TempDir::new("barged-pair").unwrap();
    let config = Config::parse(
        format!(
            r#"{{"listen":"127.0.0.1:0","mode":"lan","name":"Linux PC","state_dir":{}}}"#,
            serde_json::to_string(temp.path()).unwrap()
        )
        .as_bytes(),
    )
    .unwrap();
    let mut harness = Harness::lan(temp, config, false);
    harness.clock.at(barged::SystemClock.now_unix_ms());

    // mDNS seam advertised `_barged._tcp` with id, name, mode and protocol version.
    // Wait briefly until serve starts if needed by checking state first (which proves serve loop is running).
    let unauth = harness.client.state().unwrap_err();
    assert_eq!(unauth.kind(), barge_machine::ErrorKind::Refused);
    {
        let ads = harness.mdns.advertised.lock().unwrap();
        assert_eq!(ads.len(), 1);
        assert_eq!(ads[0].service_type, "_barged._tcp");
        assert_eq!(ads[0].instance_name, "Linux PC");
        assert_eq!(ads[0].port, harness.address.port());
        assert_eq!(
            ads[0].txt,
            vec![
                ("id".to_string(), harness.machine_id.clone()),
                ("name".to_string(), "Linux PC".to_string()),
                ("mode".to_string(), "lan".to_string()),
                ("version".to_string(), "1".to_string()),
            ]
        );
    }

    // 1. Pair first client ("MacBook") and approve via localhost.
    let keys1 = KeyPair::generate().unwrap();
    let started1 = harness
        .client
        .start_pair("MacBook", &keys1.public_key_hex())
        .unwrap();
    let derived1 = keys1
        .derive(
            &parse_public_key(&started1.public_key).unwrap(),
            PairRole::Client,
        )
        .unwrap();
    assert_eq!(
        harness
            .client
            .pair_status(&started1.request_id)
            .unwrap()
            .status,
        PairStatus::Pending
    );

    // Notification seam received the code and request_id.
    {
        let notices = harness.notifier.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0.request_id, started1.request_id);
        assert_eq!(notices[0].0.client_name, "MacBook");
        assert_eq!(notices[0].0.code, derived1.code());
    }

    let pending = harness.client.pending_pairs().unwrap();
    assert_eq!(pending.requests.len(), 1);
    assert_eq!(pending.requests[0].code, derived1.code());
    let approved1 = harness.client.approve_pair(&started1.request_id).unwrap();
    assert_eq!(approved1.client.name, "MacBook");

    let status1 = harness.client.pair_status(&started1.request_id).unwrap();
    assert_eq!(status1.status, PairStatus::Approved);
    let cid1 = status1.client_id.unwrap();
    assert_eq!(cid1, approved1.client.id);

    let client1 = DaemonClient::at(format!("http://{}", harness.address))
        .with_client_key(&cid1, derived1.key().to_vec());
    let state = client1.state().unwrap();
    assert_eq!(state.machine_id, harness.machine_id);
    assert_eq!(state.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    assert_eq!(state.broadcast, Some(Ipv4Addr::new(192, 0, 2, 255)));

    // 2. Pair second client ("Steam Deck") and approve via signed paired client (client1)!
    let keys2 = KeyPair::generate().unwrap();
    let started2 = harness
        .client
        .start_pair("Steam Deck", &keys2.public_key_hex())
        .unwrap();
    let derived2 = keys2
        .derive(
            &parse_public_key(&started2.public_key).unwrap(),
            PairRole::Client,
        )
        .unwrap();
    assert_eq!(client1.pending_pairs().unwrap().requests.len(), 1);
    let approved2 = client1.approve_pair(&started2.request_id).unwrap();
    let cid2 = approved2.client.id;
    let client2 = DaemonClient::at(format!("http://{}", harness.address))
        .with_client_key(&cid2, derived2.key().to_vec());
    assert_eq!(client2.state().unwrap().power, PowerState::Awake);

    // 3. Pair third client and deny via notification action callback.
    let keys3 = KeyPair::generate().unwrap();
    let started3 = harness
        .client
        .start_pair("Intruder", &keys3.public_key_hex())
        .unwrap();
    {
        let notices = harness.notifier.notices.lock().unwrap();
        let (notice, cb) = notices.last().unwrap();
        assert_eq!(notice.request_id, started3.request_id);
        cb(&notice.request_id, PairAction::Deny);
    }
    assert_eq!(
        harness
            .client
            .pair_status(&started3.request_id)
            .unwrap()
            .status,
        PairStatus::Denied
    );
    assert!(harness.client.approve_pair(&started3.request_id).is_err());

    // 4. Pair fourth client and let it expire after 2 minutes (120,000 ms).
    let keys4 = KeyPair::generate().unwrap();
    let started4 = harness
        .client
        .start_pair("Slow Device", &keys4.public_key_hex())
        .unwrap();
    harness.clock.advance(120_000);
    assert_eq!(
        harness
            .client
            .pair_status(&started4.request_id)
            .unwrap()
            .status,
        PairStatus::Expired
    );
    assert!(harness.client.approve_pair(&started4.request_id).is_err());
    // Re-sync the daemon clock with SystemTime before signed requests below.
    harness.clock.at(barged::SystemClock.now_unix_ms());

    // 5. List clients and revoke client1; client2 still works, client1 is rejected.
    let list = client1.clients().unwrap();
    assert_eq!(list.clients.len(), 2);
    let revoked = client2.revoke_client(&cid1).unwrap();
    assert_eq!(revoked.client_id, cid1);
    assert!(client1.state().is_err());
    assert_eq!(client2.state().unwrap().power, PowerState::Awake);
    assert_eq!(client2.clients().unwrap().clients.len(), 1);

    // Dropping the server withdraws the mDNS advertisement.
    harness.running.store(false, Ordering::SeqCst);
    harness.server.take().unwrap().join().unwrap();
    assert!(harness.mdns.dropped.load(Ordering::SeqCst));
}

#[test]
fn absent_avahi_logs_once_and_keeps_serving() {
    let temp = barge::testing::TempDir::new("barged-no-avahi").unwrap();
    let config = Config::parse(
        format!(
            r#"{{"listen":"127.0.0.1:0","mode":"lan","state_dir":{}}}"#,
            serde_json::to_string(temp.path()).unwrap()
        )
        .as_bytes(),
    )
    .unwrap();
    let harness = Harness::lan(temp, config, true);
    let keys = KeyPair::generate().unwrap();
    let started = harness
        .client
        .start_pair("MacBook", &keys.public_key_hex())
        .unwrap();
    assert_eq!(harness.mdns.advertised.lock().unwrap().len(), 1);
    assert!(!started.request_id.is_empty());
}

#[test]
fn legacy_shared_key_migrates_and_signed_requests_keep_working() {
    let temp = barge::testing::TempDir::new("barged-legacy-lan").unwrap();
    let dir = temp.path().to_path_buf();
    let key_path = dir.join("lan.key");
    let config_path = dir.join("config.json");
    let legacy_hex = "ab".repeat(32);
    auth::write_key(&key_path, &legacy_hex).unwrap();
    std::fs::write(
        &config_path,
        format!(
            r#"{{"listen":"127.0.0.1:0","mode":"lan","key_file":{}}}"#,
            serde_json::to_string(&key_path).unwrap()
        ),
    )
    .unwrap();

    let key = auth::decode(&legacy_hex, 32).unwrap();
    let config = Config::load(&config_path).unwrap();
    let clock = Arc::new(barged::SystemClock);
    let controller = PowerController::new(Arc::new(RecordingBackend::default()), clock.clone());
    let daemon =
        Arc::new(Daemon::new(&config, controller, Arc::new(FixedLookup(None)), clock).unwrap());
    let server = daemon.bind().unwrap();
    let address = server.local_addr().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let thread = thread::spawn(move || daemon.serve(server, &flag).unwrap());

    // Legacy client has NO x-barge-client-id header and keeps working seamlessly.
    let legacy_client = DaemonClient::at(format!("http://{address}")).with_key(key.clone());
    assert_eq!(legacy_client.state().unwrap().power, PowerState::Awake);
    let clients = legacy_client.clients().unwrap();
    assert_eq!(clients.clients.len(), 1);
    assert_eq!(clients.clients[0].id, "legacy");
    assert_eq!(clients.clients[0].name, "legacy");

    // Wrong key is rejected.
    assert!(DaemonClient::at(format!("http://{address}"))
        .with_key(vec![0; 32])
        .state()
        .is_err());

    // Nonce replay, timestamp skew, path/method tampering, and unsigned requests are still rejected.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let request = |timestamp: u64, nonce: &str, signed_body: &str, body: &str| {
        let ts = timestamp.to_string();
        let sig = auth::sign(
            &key,
            "POST",
            "/v1/power/cancel",
            &ts,
            nonce,
            signed_body.as_bytes(),
        )
        .unwrap();
        format!("POST /v1/power/cancel HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\nx-barge-timestamp: {ts}\r\nx-barge-nonce: {nonce}\r\nx-barge-signature: {sig}\r\n\r\n{body}", body.len())
    };
    let raw = |request: &str| {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.lines().next().unwrap().to_string()
    };
    let body = r#"{"schema":"barge.power-cancel/1"}"#;
    let nonce = "00112233445566778899aabbccddeeff";
    let valid = request(now, nonce, body, body);
    assert!(raw(&valid).contains(" 200 "));
    assert!(raw(&valid).contains(" 401 "));
    assert!(raw(&request(
        now + 300,
        "33332233445566778899aabbccddeeff",
        body,
        body
    ))
    .contains(" 401 "));
    assert!(raw(&request(now, "bad-nonce", body, body)).contains(" 401 "));
    let signed = request(now, "44442233445566778899aabbccddeeff", body, body);
    assert!(raw(&signed.replacen("/v1/power/cancel", "/v1/power", 1)).contains(" 401 "));
    assert!(raw(&signed.replacen("POST", "GET", 1)).contains(" 401 "));
    assert!(
        raw("GET /v1/state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").contains(" 401 ")
    );

    // Local `barged clients list` and `barged clients remove` work and immediately
    // revoke access on the live daemon.
    let list_out = Command::new(env!("CARGO_BIN_EXE_barged"))
        .args(["clients", "list", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(list_out.status.success());
    assert!(String::from_utf8_lossy(&list_out.stdout).contains("legacy"));

    let remove_out = Command::new(env!("CARGO_BIN_EXE_barged"))
        .args(["clients", "remove", "legacy", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(remove_out.status.success());
    assert!(legacy_client.state().is_err());

    running.store(false, Ordering::SeqCst);
    thread.join().unwrap();
}

#[test]
fn oversized_body_is_rejected() {
    let harness = Harness::new();
    let request =
        "POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Length: 16385\r\nConnection: close\r\n\r\n";
    let (status, _) = harness.raw(request);
    assert_eq!(status, 413);
}

#[test]
fn stalled_body_times_out_and_the_next_client_is_served() {
    let harness = Harness::new();
    // A completed request confirms the serving thread is running before the
    // stalled connection and its successor are queued in that order.
    assert_eq!(harness.state().power, PowerState::Awake);
    let mut slow = TcpStream::connect(harness.address).unwrap();
    slow.write_all(b"POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{")
        .unwrap();
    let start = std::time::Instant::now();
    assert_eq!(harness.state().power, PowerState::Awake);
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    let mut response = String::new();
    slow.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
}

#[test]
fn the_tailnet_listener_refuses_pairing_and_serves_a_paired_client_with_no_allowlist_entry() {
    let tailnet = FakeTailnet::default();
    tailnet.up(pc_node());
    let harness = Harness::combined(
        Some(PeerNode::new("n9", "laptop.tailnet.ts.net")),
        &[],
        tailnet,
    );
    let tailnet_address = harness.wait_for_tailnet();
    // Signed requests carry the real time, so the fake clock follows it.
    harness.clock.at(barged::SystemClock.now_unix_ms());

    // Pairing is a LAN act: the tailnet listener refuses it outright.
    let (status, body) = raw_at(
        tailnet_address,
        "POST /v1/pair HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 400);
    assert!(body.contains("served only on the LAN address"), "{body}");

    // Pair a client on the LAN, as usual.
    let keys = KeyPair::generate().unwrap();
    let started = harness
        .client
        .start_pair("MacBook", &keys.public_key_hex())
        .unwrap();
    let derived = keys
        .derive(
            &parse_public_key(&started.public_key).unwrap(),
            PairRole::Client,
        )
        .unwrap();
    let approved = harness.client.approve_pair(&started.request_id).unwrap();

    // Its signed requests work on the tailnet, though the tailnet allowlist is
    // empty: the paired client is the credential. The state names the node.
    let on_tailnet = DaemonClient::at(format!("http://{tailnet_address}"))
        .with_client_key(&approved.client.id, derived.key().to_vec());
    let state = on_tailnet.state().unwrap();
    assert_eq!(state.machine_id, harness.machine_id);
    assert_eq!(state.tailscale_name.as_deref(), Some("pc.tail1234.ts.net"));
    assert_eq!(state.tailscale_ip, Some(Ipv4Addr::new(100, 64, 0, 7)));

    // The same credentials work on the LAN, and a LAN-only state read stays
    // unsigned-refused.
    let on_lan = DaemonClient::at(format!("http://{}", harness.address))
        .with_client_key(&approved.client.id, derived.key().to_vec());
    assert_eq!(on_lan.state().unwrap().machine_id, harness.machine_id);
    assert_eq!(
        harness.client.state().unwrap_err().kind(),
        barge_machine::ErrorKind::Refused
    );

    // Clients are managed on the LAN only, even by an authenticated client.
    let error = on_tailnet.clients().unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(
        error.message().contains("served only on the LAN address"),
        "{error}"
    );

    // An unpaired caller that is not on the allowlist is refused as before.
    let error = DaemonClient::at(format!("http://{tailnet_address}"))
        .state()
        .unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(
        error.message().contains("is not on the allowlist"),
        "{error}"
    );
}

#[test]
fn an_allowlisted_tailnet_node_is_served_unsigned_and_the_allowlist_still_decides() {
    let tailnet = FakeTailnet::default();
    tailnet.up(pc_node());
    let harness = Harness::combined(
        Some(PeerNode::new("n1", "macbook.tailnet.ts.net")),
        &["macbook.tailnet.ts.net"],
        tailnet,
    );
    let tailnet_address = harness.wait_for_tailnet();
    let state = DaemonClient::at(format!("http://{tailnet_address}"))
        .state()
        .unwrap();
    assert_eq!(state.tailscale_name.as_deref(), Some("pc.tail1234.ts.net"));

    // Power is decided the same way: an allowlisted node may ask for it, and
    // may cancel it.
    let tailnet_client = DaemonClient::at(format!("http://{tailnet_address}"));
    let result = tailnet_client.power(PowerActionKind::Sleep, 15).unwrap();
    assert_eq!(result.action, PowerActionKind::Sleep);
    assert!(tailnet_client.cancel().unwrap().cancelled);
}

#[test]
fn lan_keeps_serving_while_tailscale_is_down_and_the_tailnet_listener_comes_up_later() {
    let tailnet = FakeTailnet::default();
    let temp = barge::testing::TempDir::new("barged-late-tailnet").unwrap();
    let config = Config::parse(
        format!(
            r#"{{"listen":"127.0.0.1:0","mode":"lan","name":"Linux PC","state_dir":{},"tailscale":{{"listen":"auto","allowed_nodes":["macbook.tailnet.ts.net"]}}}}"#,
            serde_json::to_string(temp.path()).unwrap()
        )
        .as_bytes(),
    )
    .unwrap();
    let harness = Harness::start(
        temp,
        config,
        Some(PeerNode::new("n1", "macbook.tailnet.ts.net")),
        false,
        Some(tailnet.clone()),
    );

    // tailscaled has no address yet: the tailnet side is not up, and the LAN
    // half answers all the same.
    thread::sleep(Duration::from_millis(30));
    assert_eq!(harness.daemon.tailnet_address(), None);
    assert!(harness.client.pending_pairs().unwrap().requests.is_empty());

    // Tailscale comes up with an IPv4 address; the listener follows.
    tailnet.up(TailnetSelf {
        dns_name: Some("pc.tail1234.ts.net".to_string()),
        ipv4: Some(Ipv4Addr::LOCALHOST),
    });
    let tailnet_address = harness.wait_for_tailnet();
    assert_eq!(tailnet_address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    let state = DaemonClient::at(format!("http://{tailnet_address}"))
        .state()
        .unwrap();
    assert_eq!(state.tailscale_name.as_deref(), Some("pc.tail1234.ts.net"));
}

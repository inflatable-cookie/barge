//! Machine registry, Wake-on-LAN, mDNS discovery, pairing and daemon client.
#![deny(missing_docs)]

pub mod auth;
pub mod base64;
pub mod daemon;
pub mod discover;
pub mod link;
pub mod pair;
mod protocol;
mod registry;
mod state;
mod wol;
pub use daemon::{request_power, AssumeYes, Confirmation, DaemonClient};
pub use discover::{
    backfill, browse_at, follow_addresses, match_registered_with, Backfilled, DiscoveredMachine,
    MatchProbe, MdnsBrowser, NetworkProbe, UdpMdnsBrowser, DEFAULT_DISCOVER_TIMEOUT,
    MDNS_SERVICE_TYPE,
};
pub use link::{remember_path, MachineLink, Reached, LAN_CONNECT_TIMEOUT};
pub use pair::{
    local_hostname, parse_public_key, verification_code, DerivedPairing, KeyPair, PairRole,
};
pub use protocol::{
    Cancel, CancelResult, ClientList, ClientRevoked, DaemonError, PairApproved, PairDecision,
    PairDenied, PairRequest, PairResponse, PairStatus, PairStatusResponse, PairedClient, Pending,
    PendingPairList, PendingPairRequest, PowerActionKind, PowerRequest, PowerResult, PowerState,
    RunCancelRequest, RunCancelResponse, RunFinishResponse, RunHaveRequest, RunHaveResponse,
    RunLogResponse, RunOutputStream, RunStartRequest, RunStartResponse, RunStreamEvent,
    RunStreamEventKind, RunUploadRequest, RunUploadResponse, State,
};
pub use registry::{sanitize_machine_name, ClientCredentials, Machine, Path, Registry};
#[cfg(not(unix))]
compile_error!("barge-machine requires Unix");

use std::net::Ipv4Addr;
pub use wol::{magic_packet, parse_mac, wake, DatagramSender, UdpSender};

/// The schema of `machines.json`.
pub const REGISTRY_SCHEMA: &str = "barge.machines/1";
/// The schema of the state `barged` reports.
pub const STATE_SCHEMA: &str = "barge.machine-state/2";
/// The schema of a power request sent to `barged`.
pub const POWER_REQUEST_SCHEMA: &str = "barge.power-request/1";
/// The schema of `barged`'s acceptance of a power request.
pub const POWER_RESULT_SCHEMA: &str = "barge.power-result/1";
/// The schema of a cancel request sent to `barged`.
pub const CANCEL_SCHEMA: &str = "barge.power-cancel/1";
/// The schema of `barged`'s answer to a cancel.
pub const CANCEL_RESULT_SCHEMA: &str = "barge.power-cancel-result/1";
/// The schema of a pairing initiation request sent to `barged`.
pub const PAIR_REQUEST_SCHEMA: &str = "barge.pair-request/1";
/// The schema of `barged`'s response to `POST /v1/pair`.
pub const PAIR_RESPONSE_SCHEMA: &str = "barge.pair-response/1";
/// The schema of `barged`'s response to `GET /v1/pair/<id>`.
pub const PAIR_STATUS_SCHEMA: &str = "barge.pair-status/1";
/// The schema of `barged`'s response to `GET /v1/pair/pending`.
pub const PAIR_PENDING_SCHEMA: &str = "barge.pair-pending/1";
/// The schema of an approve/deny decision sent to `barged`.
pub const PAIR_DECISION_SCHEMA: &str = "barge.pair-decision/1";
/// The schema of `barged`'s response to `POST /v1/pair/<id>/approve`.
pub const PAIR_APPROVED_SCHEMA: &str = "barge.pair-approved/1";
/// The schema of `barged`'s response to `POST /v1/pair/<id>/deny`.
pub const PAIR_DENIED_SCHEMA: &str = "barge.pair-denied/1";
/// The schema of `barged`'s response to `GET /v1/clients`.
pub const CLIENTS_SCHEMA: &str = "barge.clients/1";
/// The schema of `barged`'s response to `DELETE /v1/clients/<id>`.
pub const CLIENT_REVOKED_SCHEMA: &str = "barge.client-revoked/1";
/// The schema of a `POST /v1/runs/have` request.
pub const RUN_HAVE_REQUEST_SCHEMA: &str = "barge.run-have-request/1";
/// The schema of `barged`'s response to `POST /v1/runs/have`.
pub const RUN_HAVE_RESPONSE_SCHEMA: &str = "barge.run-have-response/1";
/// The schema of a `POST /v1/runs/upload` request.
pub const RUN_UPLOAD_REQUEST_SCHEMA: &str = "barge.run-upload-request/1";
/// The schema of `barged`'s response to `POST /v1/runs/upload`.
pub const RUN_UPLOAD_RESPONSE_SCHEMA: &str = "barge.run-upload-response/1";
/// The schema of a `POST /v1/runs/start` request.
pub const RUN_START_REQUEST_SCHEMA: &str = "barge.run-start-request/1";
/// The schema of `barged`'s response to `POST /v1/runs/start`.
pub const RUN_START_RESPONSE_SCHEMA: &str = "barge.run-start-response/1";
/// The schema of one event line on `GET /v1/runs/<id>/stream`.
pub const RUN_STREAM_EVENT_SCHEMA: &str = "barge.run-stream-event/1";
/// The schema of a `POST /v1/runs/<id>/cancel` request.
pub const RUN_CANCEL_REQUEST_SCHEMA: &str = "barge.run-cancel-request/1";
/// The schema of `barged`'s response to `POST /v1/runs/<id>/cancel`.
pub const RUN_CANCEL_RESPONSE_SCHEMA: &str = "barge.run-cancel-response/1";
/// The schema of `barged`'s response to `GET /v1/runs/<id>/result`.
pub const RUN_FINISH_RESPONSE_SCHEMA: &str = "barge.run-finish/1";
/// The schema of `barged`'s response to `GET /v1/runs/logs/<id>`.
pub const RUN_LOG_RESPONSE_SCHEMA: &str = "barge.run-log/1";
/// The schema of the error document every `barged` refusal carries.
pub const DAEMON_ERROR_SCHEMA: &str = "barge.daemon-error/1";
pub(crate) const DEFAULT_BROADCAST: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 255);
pub(crate) const DEFAULT_PORT: u16 = 9;
/// `barged`'s default port. The daemon's config names the address it binds; the
/// registry names the port it dials, so either side can be moved deliberately.
pub const DEFAULT_DAEMON_PORT: u16 = 9777;
/// Power commands default to the countdown MoonDeck Remote used.
pub const DEFAULT_POWER_DELAY_S: u32 = 15;
/// A countdown longer than this is a mistake, not a schedule.
pub const MAX_POWER_DELAY_S: u32 = 3600;

/// A domain error reported by the registry, a wake operation or the client.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    /// True when the request never left this machine because the connection
    /// could not be made, so another route to the same daemon may be tried.
    not_sent: bool,
}

impl Error {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            not_sent: false,
        }
    }

    /// An unreachable daemon whose request was never sent.
    pub(crate) fn unsent(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unreachable,
            message: message.into(),
            not_sent: true,
        }
    }

    /// Whether the request never reached the daemon, so retrying over another
    /// route cannot repeat an action.
    pub fn not_sent(&self) -> bool {
        self.not_sent
    }

    /// Bad input from the operator: a name, a hostname, a port or a MAC.
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    /// A well-formed request that was refused, for example a duplicate name.
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Refused, message)
    }

    /// A machine or other named record that does not exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    /// Which class of failure this is; the CLI maps it to its exit code.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human-readable explanation.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}

/// The class of a [`Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bad input from the operator.
    Usage,
    /// A well-formed request that was refused; nothing changed.
    Refused,
    /// The named record does not exist.
    NotFound,
    /// Reading, writing or sending failed.
    Io,
    /// Stored state could not be read or is not valid; nothing is assumed.
    UnknownState,
    /// The machine's daemon could not be reached. Distinct from every state a
    /// daemon can report, so an absent daemon is never read as asleep.
    Unreachable,
}

/// This crate's result type.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::NAME_RULE;
    use barge::testing::TempDir;
    use std::sync::Mutex;
    use std::{
        io,
        net::{Ipv4Addr, SocketAddr},
    };

    #[derive(Default)]
    struct FakeSender(Mutex<Vec<(Vec<u8>, SocketAddr)>>);
    impl DatagramSender for FakeSender {
        fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
            self.0.lock().unwrap().push((bytes.to_vec(), destination));
            Ok(bytes.len())
        }
    }

    /// A fresh registry directory; the `TempDir` is returned so it outlives the
    /// registry and removes itself even when a test panics.
    fn registry() -> (TempDir, Registry) {
        let temp = TempDir::new("machine-registry").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        (temp, registry)
    }

    /// One machine record as a registry entry.
    fn machine_json(name: &str, mac: &str) -> String {
        format!(
            r#"{{"name":"{name}","hostname":"pc.local","tailscale_name":null,"mac":"{mac}","broadcast":"255.255.255.255","port":9,"daemon_port":9777}}"#
        )
    }

    /// A registry document holding `machines`, as another writer would have
    /// left it.
    fn registry_json(machines: &[(&str, &str)]) -> String {
        let entries = machines
            .iter()
            .map(|(name, mac)| machine_json(name, mac))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"schema":"{REGISTRY_SCHEMA}","machines":[{entries}]}}"#)
    }

    fn document(name: &str, mac: &str) -> String {
        registry_json(&[(name, mac)])
    }

    fn write_document(temp: &TempDir, bytes: &str) {
        std::fs::write(temp.child("machines.json"), bytes).unwrap();
    }

    #[test]
    fn parses_common_macs_and_rejects_invalid_values() {
        let expected = [0, 17, 34, 51, 68, 255];
        for input in [
            "00:11:22:33:44:ff",
            "00-11-22-33-44-FF",
            "0011223344ff",
            "0011.2233.44ff",
        ] {
            assert_eq!(parse_mac(input).unwrap(), expected);
        }
        for input in [
            "",
            "0011223344",
            "0011223344gg",
            "00:11:22:33:44:55:66",
            "00-11:22:33:44:ff",
        ] {
            assert!(parse_mac(input).is_err(), "{input}");
        }
    }

    #[test]
    fn magic_packet_and_wake_destination_are_exact() {
        let mac = [0, 17, 34, 51, 68, 255];
        let packet = magic_packet(mac);
        assert_eq!(&packet[..6], &[255; 6]);
        assert!(packet[6..]
            .as_chunks::<6>()
            .0
            .iter()
            .all(|chunk| chunk == &mac));
        let machine = Machine::new("pc", "pc.local", "00:11:22:33:44:ff")
            .unwrap()
            .with_broadcast(Ipv4Addr::new(192, 0, 2, 255));
        let sender = FakeSender::default();
        assert_eq!(
            wake(&machine, &sender).unwrap(),
            (SocketAddr::from(([192, 0, 2, 255], 9)), 102)
        );
        let sent = sender.0.lock().unwrap();
        assert_eq!(sent[0].0, packet);
        assert_eq!(sent[0].1, SocketAddr::from(([192, 0, 2, 255], 9)));
    }

    #[test]
    fn wake_refuses_discovered_machine_without_mac() {
        let machine = Machine::discovered("mid-1", "pc", "192.0.2.20", 9777).unwrap();
        let sender = FakeSender::default();
        let error = wake(&machine, &sender).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Refused);
    }

    #[test]
    fn removing_a_machine_removes_its_key_before_the_name_can_be_reused() {
        let (_temp, registry) = registry();
        let machine = Machine::new("pc", "pc.local", "00:11:22:33:44:ff").unwrap();
        registry.add(machine.clone()).unwrap();
        registry.pair("pc", &"ab".repeat(32)).unwrap();
        registry.remove("pc").unwrap();
        registry.add(machine).unwrap();
        assert_eq!(registry.key("pc").unwrap(), None);
    }

    #[test]
    fn registry_round_trips_and_refuses_duplicate_names() {
        let (_temp, registry) = registry();
        let machine = Machine::new("living-room", "pc.local", "001122334455").unwrap();
        registry.add(machine.clone()).unwrap();
        assert_eq!(registry.list().unwrap(), vec![machine.clone()]);
        assert_eq!(registry.get("living-room").unwrap(), machine);
        assert_eq!(
            registry.add(machine).unwrap_err().kind(),
            ErrorKind::Refused
        );
    }

    #[test]
    fn get_and_remove_of_a_missing_name_are_not_found() {
        let (_temp, registry) = registry();
        assert_eq!(
            registry.get("absent").unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            registry.remove("absent").unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn an_unknown_registry_schema_is_unknown_state() {
        let (temp, registry) = registry();
        let bytes =
            document("pc", "00:11:22:33:44:ff").replace(REGISTRY_SCHEMA, "barge.machines/2");
        write_document(&temp, &bytes);
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("barge.machines/2"), "{error}");
    }

    #[test]
    fn corrupt_registry_json_is_unknown_state() {
        let (temp, registry) = registry();
        write_document(&temp, "{ not json");
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("cannot parse"), "{error}");
        assert_eq!(
            registry.get("pc").unwrap_err().kind(),
            ErrorKind::UnknownState,
            "a get reads the same document"
        );
    }

    #[test]
    fn duplicate_names_on_disk_are_unknown_state() {
        let (temp, registry) = registry();
        write_document(
            &temp,
            &registry_json(&[("pc", "00:11:22:33:44:ff"), ("pc", "00:11:22:33:44:ff")]),
        );
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("duplicate names"), "{error}");
    }

    #[test]
    fn a_non_canonical_mac_on_disk_is_unknown_state() {
        let (temp, registry) = registry();
        write_document(&temp, &document("pc", "00-11-22-33-44-FF"));
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("not canonical"), "{error}");
    }

    #[test]
    fn a_corrupt_name_on_disk_is_unknown_state_not_usage() {
        let (temp, registry) = registry();
        write_document(&temp, &document("not a name", "00:11:22:33:44:ff"));
        let error = registry.list().unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::UnknownState,
            "a stored record is not operator input; the CLI must exit 3, not 2"
        );
        assert!(error.message().contains(NAME_RULE), "{error}");
    }

    #[test]
    fn a_port_is_validated_where_the_machine_is_built() {
        let machine = Machine::new("pc", "pc.local", "00:11:22:33:44:ff").unwrap();
        assert_eq!(
            machine.clone().with_port(0).unwrap_err().kind(),
            ErrorKind::Usage
        );
        assert_eq!(
            machine.clone().with_daemon_port(0).unwrap_err().kind(),
            ErrorKind::Usage
        );
        let chosen = machine
            .clone()
            .with_port(7)
            .unwrap()
            .with_daemon_port(1)
            .unwrap();
        assert_eq!((chosen.port(), chosen.daemon_port()), (7, 1));
        assert_eq!(machine.port(), DEFAULT_PORT);
        assert_eq!(machine.daemon_port(), DEFAULT_DAEMON_PORT);
    }

    #[test]
    fn add_re_validates_a_record_that_bypassed_the_builders() {
        let (_temp, registry) = registry();
        let machine: Machine = serde_json::from_str(
            &machine_json("pc", "00:11:22:33:44:ff").replace("\"port\":9", "\"port\":0"),
        )
        .unwrap();
        let error = registry.add(machine).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("1-65535"), "{error}");
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn protocol_documents_round_trip_with_schema() {
        let docs = [
            serde_json::to_value(
                State::new(PowerState::Awake, 42)
                    .with_machine_id("mid-1")
                    .with_mac(Some("00:11:22:33:44:ff".to_string()))
                    .with_broadcast(Some(Ipv4Addr::new(192, 0, 2, 255)))
                    .with_pending(Some(Pending {
                        action: PowerActionKind::Sleep,
                        deadline_unix_ms: 15_042,
                    })),
            )
            .unwrap(),
            serde_json::to_value(State::new(PowerState::Asleep, 42).with_machine_id("mid-1"))
                .unwrap(),
            serde_json::to_value(PowerRequest::new(PowerActionKind::Shutdown, 15)).unwrap(),
            serde_json::to_value(PowerResult::new(PowerActionKind::Restart, 15_000)).unwrap(),
            serde_json::to_value(Cancel::default()).unwrap(),
            serde_json::to_value(CancelResult::new(true)).unwrap(),
            serde_json::to_value(DaemonError::new("not_authorized", "no")).unwrap(),
            serde_json::to_value(PairRequest::new("MacBook", "aa".repeat(32))).unwrap(),
            serde_json::to_value(PairResponse::new("req-1", "bb".repeat(32), 120_042)).unwrap(),
            serde_json::to_value(PairStatusResponse::new(
                "req-1",
                PairStatus::Approved,
                Some("cid-1".to_string()),
            ))
            .unwrap(),
            serde_json::to_value(PendingPairList::new(vec![PendingPairRequest {
                request_id: "req-1".to_string(),
                client_name: "MacBook".to_string(),
                code: "042719".to_string(),
                created_at_unix_ms: 42,
                expires_at_unix_ms: 120_042,
            }]))
            .unwrap(),
            serde_json::to_value(ClientList::new(vec![PairedClient {
                id: "cid-1".to_string(),
                name: "MacBook".to_string(),
                paired_at_unix_ms: 100,
            }]))
            .unwrap(),
            serde_json::to_value(ClientRevoked::new("cid-1")).unwrap(),
        ];
        for document in &docs {
            assert!(document.get("schema").is_some());
        }
        let state: State = serde_json::from_value(docs[0].clone()).unwrap();
        let no_pending: State = serde_json::from_value(docs[1].clone()).unwrap();
        let request: PowerRequest = serde_json::from_value(docs[2].clone()).unwrap();
        let result: PowerResult = serde_json::from_value(docs[3].clone()).unwrap();
        let cancel: Cancel = serde_json::from_value(docs[4].clone()).unwrap();
        let cancelled: CancelResult = serde_json::from_value(docs[5].clone()).unwrap();
        let error: DaemonError = serde_json::from_value(docs[6].clone()).unwrap();
        assert_eq!(state.schema, STATE_SCHEMA);
        assert_eq!(state.observed_at_unix_ms, 42);
        assert_eq!(state.machine_id, "mid-1");
        assert_eq!(state.mac.as_deref(), Some("00:11:22:33:44:ff"));
        assert_eq!(state.broadcast, Some(Ipv4Addr::new(192, 0, 2, 255)));
        assert_eq!(
            state.pending,
            Some(Pending {
                action: PowerActionKind::Sleep,
                deadline_unix_ms: 15_042,
            })
        );
        assert_eq!(no_pending.pending, None);
        assert!(docs[1].get("pending").is_none());
        assert_eq!(request.delay_s, 15);
        assert_eq!(result.schema, POWER_RESULT_SCHEMA);
        assert_eq!(result.action, PowerActionKind::Restart);
        assert_eq!(cancel, Cancel::default());
        assert_eq!(cancelled.schema, CANCEL_RESULT_SCHEMA);
        assert!(cancelled.cancelled);
        assert_eq!(error.schema, DAEMON_ERROR_SCHEMA);
        assert_eq!(error.error, "not_authorized");
        assert_eq!(serde_json::to_value(state).unwrap(), docs[0]);
        assert_eq!(serde_json::to_value(no_pending).unwrap(), docs[1]);
        assert_eq!(serde_json::to_value(request).unwrap(), docs[2]);
        assert_eq!(serde_json::to_value(result).unwrap(), docs[3]);
        assert_eq!(serde_json::to_value(cancel).unwrap(), docs[4]);
        assert_eq!(serde_json::to_value(cancelled).unwrap(), docs[5]);
        assert_eq!(serde_json::to_value(error).unwrap(), docs[6]);
    }

    #[test]
    fn run_protocol_documents_round_trip_with_schema() {
        use barge::run::{AreaUse, LogRef, Platform, RunOutcome, RunOwner, RunResult, RunStage};
        use std::collections::BTreeMap;

        let have_req =
            RunHaveRequest::new("https://example.invalid/repo.git", vec!["a".repeat(40)]);
        let have_res =
            RunHaveResponse::new("https://example.invalid/repo.git", vec!["a".repeat(40)]);
        let upload_req = RunUploadRequest::new(
            "run-1",
            "https://example.invalid/repo.git",
            Some(base64::encode(b"bundle-bytes")),
            Some(base64::encode(b"patch-bytes")),
        );
        let upload_res = RunUploadResponse::new(
            "run-1",
            "https://example.invalid/repo.git",
            true,
            Some("sha256:abcd".to_string()),
        );
        let mut env = BTreeMap::new();
        env.insert("RUST_BACKTRACE".to_string(), "1".to_string());
        let start_req = RunStartRequest::new(
            "run-1",
            "https://example.invalid/repo.git",
            "a".repeat(40),
            "b".repeat(40),
            vec!["cargo".to_string(), "test".to_string()],
            env,
            60_000,
            2,
            RunOwner::Worktree("wt-123".to_string()),
        );
        let start_res = RunStartResponse::new("run-1");
        let run_result = RunResult {
            machine: "bazzite".to_string(),
            platform: Platform {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
            },
            repo: "https://example.invalid/repo.git".to_string(),
            base: "a".repeat(40),
            patch_digest: Some("sha256:abcd".to_string()),
            tree: "b".repeat(40),
            toolchain: Some("rustc 1.100.0".to_string()),
            command: vec!["cargo".to_string(), "test".to_string()],
            started_ms: 1_000,
            finished_ms: 2_500,
            waking_ms: None,
            outcome: RunOutcome::Exited { code: 0 },
            log: LogRef {
                digest: "sha256:1234".to_string(),
                location: "/v1/runs/logs/run-1".to_string(),
            },
            area: Some(AreaUse {
                id: "slot-0123456789abcdef-2".to_string(),
                bytes: 4096,
            }),
        };
        let stage_ev = RunStreamEvent::new(
            1,
            1_000,
            RunStreamEventKind::Stage {
                stage: RunStage::Syncing,
            },
        );
        let out_ev = RunStreamEvent::new(
            2,
            1_100,
            RunStreamEventKind::Output {
                stream: RunOutputStream::Stdout,
                data_base64: base64::encode(b"ok\n"),
            },
        );
        let fin_ev = RunStreamEvent::new(
            3,
            2_500,
            RunStreamEventKind::Finished {
                result: Box::new(run_result.clone()),
            },
        );
        let cancel_req = RunCancelRequest::default();
        let cancel_res = RunCancelResponse::new("run-1", true);
        let finish_res = RunFinishResponse::new("run-1", run_result.clone());
        let log_res = RunLogResponse::new(run_result.log.clone(), base64::encode(b"full log\n"));

        fn round_trip<T>(value: &T, expected_schema: &str)
        where
            T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        {
            let json = serde_json::to_value(value).unwrap();
            assert_eq!(
                json.get("schema").and_then(|s| s.as_str()),
                Some(expected_schema)
            );
            let decoded: T = serde_json::from_value(json).unwrap();
            assert_eq!(&decoded, value);
        }

        round_trip(&have_req, RUN_HAVE_REQUEST_SCHEMA);
        round_trip(&have_res, RUN_HAVE_RESPONSE_SCHEMA);
        round_trip(&upload_req, RUN_UPLOAD_REQUEST_SCHEMA);
        round_trip(&upload_res, RUN_UPLOAD_RESPONSE_SCHEMA);
        round_trip(&start_req, RUN_START_REQUEST_SCHEMA);
        round_trip(&start_res, RUN_START_RESPONSE_SCHEMA);
        round_trip(&stage_ev, RUN_STREAM_EVENT_SCHEMA);
        round_trip(&out_ev, RUN_STREAM_EVENT_SCHEMA);
        round_trip(&fin_ev, RUN_STREAM_EVENT_SCHEMA);
        round_trip(&cancel_req, RUN_CANCEL_REQUEST_SCHEMA);
        round_trip(&cancel_res, RUN_CANCEL_RESPONSE_SCHEMA);
        round_trip(&finish_res, RUN_FINISH_RESPONSE_SCHEMA);
        round_trip(&log_res, RUN_LOG_RESPONSE_SCHEMA);
    }

    #[test]
    fn a_record_written_before_step_two_reads_the_default_daemon_port() {
        let document = serde_json::json!({
            "name": "living-room",
            "hostname": "pc.local",
            "tailscale_name": null,
            "mac": "00:11:22:33:44:ff",
            "broadcast": "255.255.255.255",
            "port": 9,
        });
        let machine: Machine = serde_json::from_value(document).unwrap();
        assert_eq!(machine.daemon_port(), DEFAULT_DAEMON_PORT);
        assert_eq!(machine.daemon_host(), "pc.local");
        let round_trip = serde_json::to_value(&machine).unwrap();
        assert_eq!(round_trip["daemon_port"], DEFAULT_DAEMON_PORT);
    }
}

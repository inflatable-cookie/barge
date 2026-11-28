//! End-to-end tests for `barged`'s remote-run endpoints and executor against a
//! local git repository fixture (no network, loopback only).

// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use barge::run::{RunOutcome, RunOwner, RunResult, RunStage};
use barge::testing::TempDir;
use barge::{Barge, FakeOs, ManualClock as BargeClock, VolumeRegistration};
use barge_machine::{
    auth, base64, DaemonError, RunCancelRequest, RunCancelResponse, RunFinishResponse,
    RunHaveRequest, RunHaveResponse, RunLogResponse, RunStartRequest, RunStartResponse,
    RunStreamEvent, RunStreamEventKind, RunUploadRequest, RunUploadResponse,
};
use barged::{
    Clock, Config, Daemon, Error, HostEnv, PeerError, PeerLookup, PeerNode, PowerBackend,
    PowerController, RunManager, RunsConfig,
};
use sha2::{Digest, Sha256};

#[derive(Debug)]
struct FixedPeer(Option<PeerNode>);

impl PeerLookup for FixedPeer {
    fn identify(&self, _peer: SocketAddr) -> std::result::Result<PeerNode, PeerError> {
        self.0
            .clone()
            .ok_or_else(|| PeerError::new("unidentified peer"))
    }
}

#[derive(Debug)]
struct NoPower;

impl PowerBackend for NoPower {
    fn perform(&self, _action: barge_machine::PowerActionKind) -> barged::Result<()> {
        Err(Error::new("no power in run tests"))
    }
}

#[derive(Debug)]
struct AtomicClock(AtomicU64);

impl AtomicClock {
    fn new(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }

    fn advance_ms(&self, delta_ms: u64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for AtomicClock {
    fn now_unix_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct TestServer {
    _temp: TempDir,
    volume_mount: PathBuf,
    clock: Arc<AtomicClock>,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl TestServer {
    fn start(runs_enabled: bool, log_retention_s: u64, host_env: Option<HostEnv>) -> Self {
        let temp = TempDir::new("barged-runs").unwrap();
        let state_dir = temp.child("barged-state");
        let barge_state = temp.child("barge-state");
        let volume_mount = temp.child("volume");
        fs::create_dir_all(&volume_mount).unwrap();

        let fake_os = Arc::new(FakeOs::with_volume(
            &volume_mount,
            "11111111-2222-3333-4444-555555555555",
            50 << 30,
        ));
        let barge_clock = Arc::new(BargeClock::new(1_700_000_000_000));
        let barge = Barge::open_with(&barge_state, fake_os, barge_clock).unwrap();
        barge
            .register_volume(
                VolumeRegistration::new("Builds", &volume_mount)
                    .reserve_bytes(1 << 30)
                    .floor_bytes(64 << 20),
            )
            .unwrap();

        let runs_cfg = RunsConfig {
            enabled: runs_enabled,
            volume: runs_enabled.then(|| "Builds".to_string()),
            barge_state_dir: Some(barge_state),
            log_retention_s,
        };
        let config_json = serde_json::json!({
            "listen": "127.0.0.1:0",
            "allowed_nodes": ["mac.tailnet.ts.net"],
            "state_dir": state_dir,
            "runs": runs_cfg,
        });
        let config = Config::parse(&serde_json::to_vec(&config_json).unwrap()).unwrap();
        let clock = Arc::new(AtomicClock::new(1_700_000_000_000));
        let controller = PowerController::new(Arc::new(NoPower), clock.clone());
        let mut run_manager =
            RunManager::new(&config.runs, "test-pc".to_string(), clock.clone()).with_barge(barge);
        if let Some(env) = host_env {
            run_manager = run_manager.with_host_env(env);
        }
        let daemon = Daemon::new(
            &config,
            controller,
            Arc::new(FixedPeer(Some(PeerNode::new("n1", "mac.tailnet.ts.net")))),
            clock.clone(),
        )
        .unwrap()
        .with_runs(Arc::new(run_manager));

        let listener = daemon.bind().unwrap();
        let addr = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let handle = thread::spawn(move || {
            daemon.serve(listener, &flag).unwrap();
        });

        Self {
            _temp: temp,
            volume_mount,
            clock,
            addr,
            running,
            handle: Some(handle),
        }
    }

    fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> (u16, String) {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let body_bytes = body.unwrap_or(&[]);
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body_bytes.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(body_bytes).unwrap();
        stream.flush().unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, resp_body) = response.split_once("\r\n\r\n").unwrap();
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        (status, resp_body.to_string())
    }

    fn post_json<Req: serde::Serialize>(&self, path: &str, doc: &Req) -> (u16, String) {
        let bytes = serde_json::to_vec(doc).unwrap();
        self.request("POST", path, Some(&bytes))
    }

    fn stream_events(&self, path: &str) -> (u16, Vec<RunStreamEvent>) {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).unwrap();
        let status = status_line
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" || header.is_empty() {
                break;
            }
        }
        let mut events = Vec::new();
        loop {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line).unwrap() == 0 {
                break;
            }
            let size = usize::from_str_radix(size_line.trim(), 16).unwrap();
            if size == 0 {
                break;
            }
            let mut chunk = vec![0_u8; size];
            reader.read_exact(&mut chunk).unwrap();
            let mut crlf = [0_u8; 2];
            reader.read_exact(&mut crlf).unwrap();
            let line = std::str::from_utf8(&chunk).unwrap().trim();
            if !line.is_empty() {
                events.push(serde_json::from_str::<RunStreamEvent>(line).unwrap());
            }
        }
        (status, events)
    }

    fn wait_for_result(&self, request_id: &str) -> RunResult {
        let deadline = Instant::now() + Duration::from_secs(10);
        let path = format!("/v1/runs/{request_id}/result");
        while Instant::now() < deadline {
            let (status, body) = self.request("GET", &path, None);
            if status == 200 {
                let parsed: RunFinishResponse = serde_json::from_str(&body).unwrap();
                return parsed.result;
            }
            assert_eq!(status, 409, "unexpected status {status}: {body}");
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for run {request_id}");
    }
}

struct GitFixture {
    _temp: TempDir,
    dir: PathBuf,
    head: String,
    tree: String,
    bundle: Vec<u8>,
}

impl GitFixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = TempDir::new("barged-git-fixture").unwrap();
        let dir = temp.child("repo");
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "user.email", "test@example.invalid"]);
        for (rel, content) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "initial", "--quiet"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let tree = git_out(&dir, &["write-tree"]);
        let bundle_path = temp.child("repo.bundle");
        git(
            &dir,
            &["bundle", "create", bundle_path.to_str().unwrap(), "HEAD"],
        );
        let bundle = fs::read(&bundle_path).unwrap();
        Self {
            _temp: temp,
            dir,
            head,
            tree,
            bundle,
        }
    }

    fn make_patch(&self, rel: &str, new_content: &str) -> (Vec<u8>, String) {
        fs::write(self.dir.join(rel), new_content).unwrap();
        let patch = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["diff", "--binary"])
            .output()
            .unwrap();
        assert!(patch.status.success());
        git(&self.dir, &["add", rel]);
        let dirty_tree = git_out(&self.dir, &["write-tree"]);
        (patch.stdout, dirty_tree)
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn owner() -> RunOwner {
    RunOwner::Worktree("wt-test-1".to_string())
}

#[test]
fn runs_disabled_by_default_refuses_all_run_endpoints() {
    let server = TestServer::start(false, 604_800, None);

    let (status, body) = server.post_json(
        "/v1/runs/have",
        &RunHaveRequest::new("https://example.invalid/r.git", vec!["a".repeat(40)]),
    );
    assert_eq!(status, 403);
    let err: DaemonError = serde_json::from_str(&body).unwrap();
    assert_eq!(err.error, "runs_disabled");

    let (status, _) = server.post_json(
        "/v1/runs/start",
        &RunStartRequest::new(
            "run-disabled",
            "https://example.invalid/r.git",
            "a".repeat(40),
            "b".repeat(40),
            vec!["/bin/echo".to_string(), "hi".to_string()],
            BTreeMap::new(),
            5_000,
            0,
            owner(),
        ),
    );
    assert_eq!(status, 403);

    let (status, _) = server.request("GET", "/v1/runs/logs/run-disabled", None);
    assert_eq!(status, 403);
}

#[test]
fn have_upload_patch_apply_stream_log_and_retention() {
    let host_env = HostEnv {
        home: PathBuf::from("/tmp/barge-test-home"),
        path: "/usr/bin:/bin".to_string(),
        user: "barge-test-user".to_string(),
    };
    let server = TestServer::start(true, 60, Some(host_env));
    let fixture = GitFixture::new(&[("hello.txt", "before\n")]);
    let (patch, dirty_tree) = fixture.make_patch("hello.txt", "after-patch\n");
    let repo = "https://example.invalid/patch-repo.git";

    // Before upload: mirror does not have the commit.
    let (status, body) = server.post_json(
        "/v1/runs/have",
        &RunHaveRequest::new(repo, vec![fixture.head.clone()]),
    );
    assert_eq!(status, 200, "{body}");
    let have: RunHaveResponse = serde_json::from_str(&body).unwrap();
    assert!(have.have.is_empty());

    // Upload bundle + binary patch.
    let (status, body) = server.post_json(
        "/v1/runs/upload",
        &RunUploadRequest::new(
            "run-patch-1",
            repo,
            Some(base64::encode(&fixture.bundle)),
            Some(base64::encode(&patch)),
        ),
    );
    assert_eq!(status, 200, "{body}");
    let uploaded: RunUploadResponse = serde_json::from_str(&body).unwrap();
    assert!(uploaded.bundle_applied);
    assert!(uploaded.patch_digest.is_some());

    // After upload: mirror reports the commit.
    let (status, body) = server.post_json(
        "/v1/runs/have",
        &RunHaveRequest::new(repo, vec![fixture.head.clone()]),
    );
    assert_eq!(status, 200);
    let have: RunHaveResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(have.have, vec![fixture.head.clone()]);

    // Start run verifying patched file, allowlisted env, and CARGO_TARGET_DIR placement.
    let mut env = BTreeMap::new();
    env.insert("CUSTOM_ALLOW".to_string(), "allowed-value".to_string());
    let (status, body) = server.post_json(
        "/v1/runs/start",
        &RunStartRequest::new(
            "run-patch-1",
            repo,
            fixture.head.clone(),
            dirty_tree.clone(),
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "cat hello.txt; printf 'ALLOW=%s\\nTARGET=%s\\nPATH=%s\\n' \"$CUSTOM_ALLOW\" \"$CARGO_TARGET_DIR\" \"$PATH\"".to_string(),
            ],
            env,
            10_000,
            0,
            owner(),
        ),
    );
    assert_eq!(status, 202, "{body}");
    let started: RunStartResponse = serde_json::from_str(&body).unwrap();

    // Stream live events over chunked HTTP.
    let (status, events) = server.stream_events(&started.stream_path);
    assert_eq!(status, 200);
    let stages: Vec<RunStage> = events
        .iter()
        .filter_map(|e| match &e.event {
            RunStreamEventKind::Stage { stage } => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        vec![RunStage::Syncing, RunStage::Toolchain, RunStage::Running]
    );
    let finished = events
        .iter()
        .find_map(|e| match &e.event {
            RunStreamEventKind::Finished { result } => Some((**result).clone()),
            _ => None,
        })
        .expect("stream ends with Finished event");
    assert_eq!(finished.outcome, RunOutcome::Exited { code: 0 });
    assert_eq!(finished.tree, dirty_tree);
    assert_eq!(finished.patch_digest, uploaded.patch_digest);
    assert!(finished.area.is_some());

    // Fetch retained log and verify digest & output contents.
    let (status, body) = server.request("GET", &finished.log.location, None);
    assert_eq!(status, 200, "{body}");
    let log_doc: RunLogResponse = serde_json::from_str(&body).unwrap();
    let raw_log = base64::decode(&log_doc.content_base64).unwrap();
    let expected_digest = format!("sha256:{}", auth::hex(&Sha256::digest(&raw_log)));
    assert_eq!(log_doc.log.digest, expected_digest);
    assert_eq!(log_doc.log.digest, finished.log.digest);
    let text = String::from_utf8(raw_log).unwrap();
    assert!(text.contains("after-patch"), "{text}");
    assert!(text.contains("ALLOW=allowed-value"), "{text}");
    assert!(
        text.contains(&format!("TARGET={}", server.volume_mount.display())),
        "{text}"
    );
    assert!(
        text.contains("PATH=/tmp/barge-test-home/.cargo/bin:"),
        "{text}"
    );

    // Advance clock past log_retention_s (60s) and verify log is pruned.
    server.clock.advance_ms(61_000);
    let (status, _) = server.request("GET", &finished.log.location, None);
    assert_eq!(status, 404);
}

#[test]
fn tree_mismatch_and_submodules_and_lfs_fail_at_syncing() {
    let server = TestServer::start(true, 604_800, None);

    // 1. Tree mismatch
    let fixture = GitFixture::new(&[("a.txt", "hello\n")]);
    let repo = "https://example.invalid/mismatch.git";
    let (status, _) = server.post_json(
        "/v1/runs/upload",
        &RunUploadRequest::new(
            "run-mismatch",
            repo,
            Some(base64::encode(&fixture.bundle)),
            None,
        ),
    );
    assert_eq!(status, 200);
    let (status, _) = server.post_json(
        "/v1/runs/start",
        &RunStartRequest::new(
            "run-mismatch",
            repo,
            fixture.head.clone(),
            "0".repeat(40),
            vec!["/bin/echo".to_string(), "never".to_string()],
            BTreeMap::new(),
            5_000,
            0,
            owner(),
        ),
    );
    assert_eq!(status, 202);
    let result = server.wait_for_result("run-mismatch");
    match result.outcome {
        RunOutcome::Infrastructure { stage, reason } => {
            assert_eq!(stage, RunStage::Syncing);
            assert!(reason.contains("tree mismatch"), "{reason}");
        }
        other => panic!("expected Syncing Infrastructure, got {other:?}"),
    }

    // 2. Submodules (.gitmodules)
    let sub_fixture = GitFixture::new(&[(".gitmodules", "[submodule \"x\"]\n\tpath = x\n")]);
    let sub_repo = "https://example.invalid/submodules.git";
    assert_eq!(
        server
            .post_json(
                "/v1/runs/upload",
                &RunUploadRequest::new(
                    "run-sub",
                    sub_repo,
                    Some(base64::encode(&sub_fixture.bundle)),
                    None,
                ),
            )
            .0,
        200
    );
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-sub",
                    sub_repo,
                    sub_fixture.head.clone(),
                    sub_fixture.tree.clone(),
                    vec!["/bin/echo".to_string(), "never".to_string()],
                    BTreeMap::new(),
                    5_000,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );
    let result = server.wait_for_result("run-sub");
    match result.outcome {
        RunOutcome::Infrastructure { stage, reason } => {
            assert_eq!(stage, RunStage::Syncing);
            assert!(reason.contains("submodule"), "{reason}");
        }
        other => panic!("expected Syncing Infrastructure for submodules, got {other:?}"),
    }

    // 3. Git LFS pointer
    let lfs_fixture = GitFixture::new(&[(
        "asset.bin",
        "version https://git-lfs.github.com/spec/v1\noid sha256:0000\nsize 123\n",
    )]);
    let lfs_repo = "https://example.invalid/lfs.git";
    assert_eq!(
        server
            .post_json(
                "/v1/runs/upload",
                &RunUploadRequest::new(
                    "run-lfs",
                    lfs_repo,
                    Some(base64::encode(&lfs_fixture.bundle)),
                    None,
                ),
            )
            .0,
        200
    );
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-lfs",
                    lfs_repo,
                    lfs_fixture.head.clone(),
                    lfs_fixture.tree.clone(),
                    vec!["/bin/echo".to_string(), "never".to_string()],
                    BTreeMap::new(),
                    5_000,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );
    let result = server.wait_for_result("run-lfs");
    match result.outcome {
        RunOutcome::Infrastructure { stage, reason } => {
            assert_eq!(stage, RunStage::Syncing);
            assert!(reason.contains("LFS"), "{reason}");
        }
        other => panic!("expected Syncing Infrastructure for LFS, got {other:?}"),
    }
}

#[test]
fn budget_expiry_cancel_disconnect_and_signal_death() {
    let server = TestServer::start(true, 604_800, None);
    let fixture = GitFixture::new(&[("a.txt", "ok\n")]);
    let repo = "https://example.invalid/lifecycle.git";
    assert_eq!(
        server
            .post_json(
                "/v1/runs/upload",
                &RunUploadRequest::new(
                    "seed-lifecycle",
                    repo,
                    Some(base64::encode(&fixture.bundle)),
                    None,
                ),
            )
            .0,
        200
    );

    // 1. Budget expiry
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-budget",
                    repo,
                    fixture.head.clone(),
                    fixture.tree.clone(),
                    vec!["/bin/sleep".to_string(), "30".to_string()],
                    BTreeMap::new(),
                    150,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );
    assert_eq!(
        server.wait_for_result("run-budget").outcome,
        RunOutcome::BudgetExpired
    );

    // 2. Explicit cancel via POST /v1/runs/<id>/cancel
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-cancel",
                    repo,
                    fixture.head.clone(),
                    fixture.tree.clone(),
                    vec!["/bin/sleep".to_string(), "30".to_string()],
                    BTreeMap::new(),
                    30_000,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );
    // Wait until the stream shows Running, or cancel immediately.
    let (status, body) =
        server.post_json("/v1/runs/run-cancel/cancel", &RunCancelRequest::default());
    assert_eq!(status, 200, "{body}");
    let cancelled: RunCancelResponse = serde_json::from_str(&body).unwrap();
    assert!(cancelled.cancelled);
    assert_eq!(
        server.wait_for_result("run-cancel").outcome,
        RunOutcome::Cancelled
    );

    // 3. Caller disconnect on /stream cancels the run.
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-disconnect",
                    repo,
                    fixture.head.clone(),
                    fixture.tree.clone(),
                    vec!["/bin/sleep".to_string(), "30".to_string()],
                    BTreeMap::new(),
                    30_000,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );
    {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        stream
            .write_all(
                b"GET /v1/runs/run-disconnect/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let mut buf = [0_u8; 64];
        let _ = stream.read(&mut buf);
        // Dropping `stream` closes the TCP connection.
    }
    assert_eq!(
        server.wait_for_result("run-disconnect").outcome,
        RunOutcome::Cancelled
    );

    // 4. Signal death (SIGKILL = 9)
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-signal",
                    repo,
                    fixture.head.clone(),
                    fixture.tree.clone(),
                    vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        "kill -KILL $$".to_string(),
                    ],
                    BTreeMap::new(),
                    10_000,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );
    assert_eq!(
        server.wait_for_result("run-signal").outcome,
        RunOutcome::Signalled {
            signal: libc::SIGKILL
        }
    );
}

#[test]
fn refuses_env_override_and_concurrent_same_slot() {
    let server = TestServer::start(true, 604_800, None);
    let fixture = GitFixture::new(&[("a.txt", "ok\n")]);
    let repo = "https://example.invalid/slot-env.git";
    assert_eq!(
        server
            .post_json(
                "/v1/runs/upload",
                &RunUploadRequest::new(
                    "seed-slot",
                    repo,
                    Some(base64::encode(&fixture.bundle)),
                    None,
                ),
            )
            .0,
        200
    );

    // Refuses CARGO_TARGET_DIR override in env allowlist.
    for forbidden in ["CARGO_TARGET_DIR", "TMPDIR", "HOME", "PATH"] {
        let mut env = BTreeMap::new();
        env.insert(forbidden.to_string(), "/tmp/evil".to_string());
        let (status, body) = server.post_json(
            "/v1/runs/start",
            &RunStartRequest::new(
                format!("run-env-{forbidden}"),
                repo,
                fixture.head.clone(),
                fixture.tree.clone(),
                vec!["/bin/echo".to_string(), "hi".to_string()],
                env,
                5_000,
                0,
                owner(),
            ),
        );
        assert_eq!(status, 400, "{forbidden}: {body}");
        assert!(body.contains(forbidden), "{body}");
    }

    // Slot exclusivity: slot 0 active refuses second slot 0, allows slot 1.
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-slot0-a",
                    repo,
                    fixture.head.clone(),
                    fixture.tree.clone(),
                    vec!["/bin/sleep".to_string(), "30".to_string()],
                    BTreeMap::new(),
                    30_000,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );

    let (status, body) = server.post_json(
        "/v1/runs/start",
        &RunStartRequest::new(
            "run-slot0-b",
            repo,
            fixture.head.clone(),
            fixture.tree.clone(),
            vec!["/bin/echo".to_string(), "nope".to_string()],
            BTreeMap::new(),
            5_000,
            0,
            owner(),
        ),
    );
    assert_eq!(status, 409, "{body}");
    let err: DaemonError = serde_json::from_str(&body).unwrap();
    assert_eq!(err.error, "slot_busy");

    // Different slot (slot 1) is accepted while slot 0 is busy.
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-slot1",
                    repo,
                    fixture.head.clone(),
                    fixture.tree.clone(),
                    vec!["/bin/echo".to_string(), "slot1-ok".to_string()],
                    BTreeMap::new(),
                    5_000,
                    1,
                    owner(),
                ),
            )
            .0,
        202
    );
    assert_eq!(
        server.wait_for_result("run-slot1").outcome,
        RunOutcome::Exited { code: 0 }
    );

    // Clean up slot 0.
    assert_eq!(
        server
            .post_json("/v1/runs/run-slot0-a/cancel", &RunCancelRequest::default())
            .0,
        200
    );
    assert_eq!(
        server.wait_for_result("run-slot0-a").outcome,
        RunOutcome::Cancelled
    );
}

#[test]
fn missing_rustup_reports_toolchain_infrastructure() {
    let empty_env = HostEnv {
        home: PathBuf::from("/nonexistent/barge-no-rustup"),
        path: String::new(),
        user: "nobody".to_string(),
    };
    let server = TestServer::start(true, 604_800, Some(empty_env));
    let fixture = GitFixture::new(&[(
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"1.100.0\"\n",
    )]);
    let repo = "https://example.invalid/toolchain-missing.git";

    assert_eq!(
        server
            .post_json(
                "/v1/runs/upload",
                &RunUploadRequest::new(
                    "run-tc",
                    repo,
                    Some(base64::encode(&fixture.bundle)),
                    None,
                ),
            )
            .0,
        200
    );
    assert_eq!(
        server
            .post_json(
                "/v1/runs/start",
                &RunStartRequest::new(
                    "run-tc",
                    repo,
                    fixture.head.clone(),
                    fixture.tree.clone(),
                    vec!["/bin/echo".to_string(), "never".to_string()],
                    BTreeMap::new(),
                    5_000,
                    0,
                    owner(),
                ),
            )
            .0,
        202
    );
    let result = server.wait_for_result("run-tc");
    match result.outcome {
        RunOutcome::Infrastructure { stage, reason } => {
            assert_eq!(stage, RunStage::Toolchain);
            assert!(reason.contains("rust toolchain unavailable"), "{reason}");
        }
        other => panic!("expected Toolchain Infrastructure, got {other:?}"),
    }
}

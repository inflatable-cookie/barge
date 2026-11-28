//! Remote run executor: source mirror/patch sync, Barge build areas, slot
//! exclusivity, live event streaming, cancellation and bounded log retention.

pub mod exec;
mod git;
mod logs;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use barge::run::{AreaUse, LogRef, Platform, RunOutcome, RunResult, RunStage};
use barge::{Absent, Area, AreaSpec, Barge, ErrorKind as BargeErrorKind, Mode};
use barge_machine::{
    auth, base64, RunCancelResponse, RunFinishResponse, RunHaveRequest, RunHaveResponse,
    RunLogResponse, RunOutputStream, RunStartRequest, RunStartResponse, RunStreamEvent,
    RunStreamEventKind, RunUploadRequest, RunUploadResponse,
};
use sha2::{Digest, Sha256};

use crate::config::RunsConfig;
use crate::runs::exec::HostEnv;
use crate::runs::logs::{LogStore, LogWriter};
use crate::Clock;

const AREA_NAMESPACE: &str = "barged";
const MIRRORS_AREA: &str = "mirrors";
const BUILDS_AREA: &str = "builds";
const LOGS_AREA: &str = "logs";
const LOGS_ROOT_ID: &str = "run-logs";

/// Error returned by [`RunManager`] methods, mapped directly to HTTP status codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl RunError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "bad_request",
            message: message.into(),
        }
    }

    pub(crate) fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: 403,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: 404,
            code: "not_found",
            message: message.into(),
        }
    }

    pub(crate) fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: 409,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 500,
            code: "internal",
            message: message.into(),
        }
    }
}

/// Live state and event stream of one run.
pub(crate) struct RunHandle {
    state: Mutex<RunStreamState>,
    notify: Condvar,
    cancelled: Arc<AtomicBool>,
}

struct RunStreamState {
    events: Vec<RunStreamEvent>,
    result: Option<RunResult>,
}

impl fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("RunHandle")
            .field("events", &state.events.len())
            .field("finished", &state.result.is_some())
            .field("cancelled", &self.cancelled.load(Ordering::SeqCst))
            .finish()
    }
}

impl RunHandle {
    fn new() -> Self {
        Self {
            state: Mutex::new(RunStreamState {
                events: Vec::new(),
                result: None,
            }),
            notify: Condvar::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal cancellation to the running command's process group.
    /// Returns true if the run was still active when cancelled.
    pub(crate) fn cancel(&self) -> bool {
        let finished = lock(&self.state).result.is_some();
        if finished {
            return false;
        }
        self.cancelled.store(true, Ordering::SeqCst);
        true
    }

    pub(crate) fn is_finished(&self) -> bool {
        lock(&self.state).result.is_some()
    }

    pub(crate) fn result(&self) -> Option<RunResult> {
        lock(&self.state).result.clone()
    }

    /// Wait up to `timeout` for events starting at `cursor`.
    /// Returns `(new_events, finished)`.
    pub(crate) fn read_events_from(
        &self,
        cursor: usize,
        timeout: Duration,
    ) -> (Vec<RunStreamEvent>, bool) {
        let mut guard = lock(&self.state);
        if cursor >= guard.events.len() && guard.result.is_none() {
            let (next_guard, _) = self
                .notify
                .wait_timeout(guard, timeout)
                .unwrap_or_else(|e| e.into_inner());
            guard = next_guard;
        }
        let slice = if cursor < guard.events.len() {
            guard.events[cursor..].to_vec()
        } else {
            Vec::new()
        };
        let finished = guard.result.is_some() && cursor + slice.len() >= guard.events.len();
        (slice, finished)
    }

    fn push_event(&self, timestamp_unix_ms: u64, kind: RunStreamEventKind) {
        let mut guard = lock(&self.state);
        let seq = (guard.events.len() as u64) + 1;
        guard
            .events
            .push(RunStreamEvent::new(seq, timestamp_unix_ms, kind));
        self.notify.notify_all();
    }

    fn finish(&self, timestamp_unix_ms: u64, result: RunResult) {
        let mut guard = lock(&self.state);
        guard.result = Some(result.clone());
        let seq = (guard.events.len() as u64) + 1;
        guard.events.push(RunStreamEvent::new(
            seq,
            timestamp_unix_ms,
            RunStreamEventKind::Finished {
                result: Box::new(result),
            },
        ));
        self.notify.notify_all();
    }
}

#[derive(Debug, Clone)]
struct StagedPatch {
    repo_hash: String,
    digest: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct Registry {
    active_slots: HashSet<(String, u32)>,
    runs: HashMap<String, Arc<RunHandle>>,
    staged_patches: HashMap<String, StagedPatch>,
}

/// Manages remote runs, Barge storage areas, mirror locking, slot exclusivity
/// and retained logs.
pub struct RunManager {
    enabled: bool,
    volume: Option<String>,
    barge_state_dir: Option<PathBuf>,
    barge_override: Option<Barge>,
    machine_name: String,
    log_retention_s: u64,
    clock: Arc<dyn Clock>,
    host_env: HostEnv,
    registry: Arc<Mutex<Registry>>,
    mirror_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl fmt::Debug for RunManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunManager")
            .field("enabled", &self.enabled)
            .field("volume", &self.volume)
            .field("machine_name", &self.machine_name)
            .field("log_retention_s", &self.log_retention_s)
            .finish_non_exhaustive()
    }
}

impl RunManager {
    pub fn new(config: &RunsConfig, machine_name: String, clock: Arc<dyn Clock>) -> Self {
        Self {
            enabled: config.enabled,
            volume: config.volume.clone(),
            barge_state_dir: config.barge_state_dir.clone(),
            barge_override: None,
            machine_name,
            log_retention_s: config.log_retention_s,
            clock,
            host_env: HostEnv::from_process(),
            registry: Arc::new(Mutex::new(Registry::default())),
            mirror_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Inject a [`Barge`] instance (e.g. with `FakeOs` and `TempDir`) for tests.
    pub fn with_barge(mut self, barge: Barge) -> Self {
        self.barge_override = Some(barge);
        self
    }

    /// Inject `HOME`, `PATH` and `USER` for deterministic tests.
    pub fn with_host_env(mut self, host_env: HostEnv) -> Self {
        self.host_env = host_env;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn require_enabled(&self) -> std::result::Result<&str, RunError> {
        if !self.enabled {
            return Err(RunError::forbidden(
                "runs_disabled",
                "remote runs are disabled in barged configuration",
            ));
        }
        self.volume.as_deref().ok_or_else(|| {
            RunError::internal("runs.enabled is true but runs.volume is not configured")
        })
    }

    fn open_barge(&self) -> std::result::Result<Barge, RunError> {
        if let Some(barge) = &self.barge_override {
            return Ok(barge.clone());
        }
        match &self.barge_state_dir {
            Some(dir) => Barge::open_with(
                dir.clone(),
                barge::os::platform(),
                Arc::new(barge::SystemClock::new()),
            )
            .map_err(|error| RunError::internal(format!("cannot open Barge state: {error}"))),
            None => Barge::open()
                .map_err(|error| RunError::internal(format!("cannot open Barge state: {error}"))),
        }
    }

    fn mirror_lock(&self, repo_hash: &str) -> Arc<Mutex<()>> {
        let mut locks = lock(&self.mirror_locks);
        Arc::clone(
            locks
                .entry(repo_hash.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Handle `POST /v1/runs/have`.
    pub fn have(&self, request: &RunHaveRequest) -> std::result::Result<RunHaveResponse, RunError> {
        let volume = self.require_enabled()?;
        validate_repo(&request.repo)?;
        for commit in &request.commits {
            git::validate_commit_sha(commit).map_err(|e| RunError::bad_request(e.to_string()))?;
        }
        let barge = self.open_barge()?;
        let area = declare_area(&barge, volume, MIRRORS_AREA)?;
        let repo_hash = git::repo_hash(&request.repo);
        let root_id = format!("mirror-{repo_hash}");
        let mirror_mutex = self.mirror_lock(&repo_hash);
        let _guard = lock(&mirror_mutex);

        let have = match area.resolve(&root_id) {
            Ok(root) => git::commits_present(&root.path, &request.commits)
                .map_err(|e| RunError::internal(e.to_string()))?,
            Err(error) if error.kind() == BargeErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(RunError::internal(format!(
                    "cannot inspect mirror: {error}"
                )))
            }
        };
        Ok(RunHaveResponse::new(request.repo.clone(), have))
    }

    /// Handle `POST /v1/runs/upload`.
    pub fn upload(
        &self,
        request: &RunUploadRequest,
    ) -> std::result::Result<RunUploadResponse, RunError> {
        let volume = self.require_enabled()?;
        validate_request_id(&request.request_id)?;
        validate_repo(&request.repo)?;

        let bundle_bytes = match &request.bundle_base64 {
            Some(encoded) => Some(
                base64::decode(encoded)
                    .ok_or_else(|| RunError::bad_request("bundle_base64 is not valid base64"))?,
            ),
            None => None,
        };
        let patch_bytes = match &request.patch_base64 {
            Some(encoded) => Some(
                base64::decode(encoded)
                    .ok_or_else(|| RunError::bad_request("patch_base64 is not valid base64"))?,
            ),
            None => None,
        };

        let repo_hash = git::repo_hash(&request.repo);
        let mut bundle_applied = false;
        if let Some(bytes) = bundle_bytes {
            let barge = self.open_barge()?;
            let area = declare_area(&barge, volume, MIRRORS_AREA)?;
            let root_id = format!("mirror-{repo_hash}");
            let mirror_mutex = self.mirror_lock(&repo_hash);
            let _guard = lock(&mirror_mutex);
            let root = ensure_adopted_root(&area, &root_id, "mirror")?;
            let _hold = area
                .hold(&root.id, Mode::Exclusive)
                .map_err(|e| RunError::internal(format!("cannot hold mirror root: {e}")))?;
            git::apply_bundle(&root.path, &request.request_id, &bytes)
                .map_err(|e| RunError::bad_request(format!("cannot apply git bundle: {e}")))?;
            bundle_applied = true;
        }

        let patch_digest = if let Some(bytes) = patch_bytes {
            let digest = format!("sha256:{}", auth::hex(&Sha256::digest(&bytes)));
            let mut reg = lock(&self.registry);
            reg.staged_patches.insert(
                request.request_id.clone(),
                StagedPatch {
                    repo_hash,
                    digest: digest.clone(),
                    bytes,
                },
            );
            Some(digest)
        } else {
            None
        };

        Ok(RunUploadResponse::new(
            request.request_id.clone(),
            request.repo.clone(),
            bundle_applied,
            patch_digest,
        ))
    }

    /// Handle `POST /v1/runs/start`. Validates inputs, enforces slot
    /// exclusivity, and spawns the background executor thread.
    pub fn start(
        self: &Arc<Self>,
        request: RunStartRequest,
    ) -> std::result::Result<RunStartResponse, RunError> {
        let volume = self.require_enabled()?.to_string();
        validate_request_id(&request.request_id)?;
        validate_repo(&request.repo)?;
        git::validate_commit_sha(&request.base)
            .map_err(|e| RunError::bad_request(e.to_string()))?;
        if request.expected_tree.trim().is_empty() {
            return Err(RunError::bad_request("expected_tree must not be empty"));
        }
        if request.command.is_empty() || request.command[0].trim().is_empty() {
            return Err(RunError::bad_request("command argv must not be empty"));
        }
        if request.budget_ms == 0 {
            return Err(RunError::bad_request("budget_ms must be greater than zero"));
        }
        exec::validate_caller_env(&request.env)
            .map_err(|e| RunError::bad_request(e.to_string()))?;

        let repo_hash = git::repo_hash(&request.repo);
        let slot_key = (repo_hash.clone(), request.slot);

        let (handle, staged_patch) = {
            let mut reg = lock(&self.registry);
            if reg.runs.contains_key(&request.request_id) {
                return Err(RunError::conflict(
                    "duplicate_run",
                    format!("run {:?} already exists", request.request_id),
                ));
            }
            if reg.active_slots.contains(&slot_key) {
                return Err(RunError::conflict(
                    "slot_busy",
                    format!(
                        "repo {:?} slot {} already has an active run",
                        request.repo, request.slot
                    ),
                ));
            }
            let staged_patch = reg
                .staged_patches
                .remove(&request.request_id)
                .filter(|patch| patch.repo_hash == repo_hash);
            let handle = Arc::new(RunHandle::new());
            reg.active_slots.insert(slot_key.clone());
            reg.runs
                .insert(request.request_id.clone(), Arc::clone(&handle));
            (handle, staged_patch)
        };

        let response = RunStartResponse::new(request.request_id.clone());
        let manager = Arc::clone(self);
        thread::spawn(move || {
            let _slot_guard = SlotGuard {
                registry: Arc::clone(&manager.registry),
                slot_key,
            };
            manager.execute_run(&volume, request, repo_hash, staged_patch, handle);
        });

        Ok(response)
    }

    /// Look up a run handle for streaming (`GET /v1/runs/<id>/stream`).
    pub(crate) fn stream_handle(
        &self,
        request_id: &str,
    ) -> std::result::Result<Arc<RunHandle>, RunError> {
        let _ = self.require_enabled()?;
        validate_request_id(request_id)?;
        let reg = lock(&self.registry);
        reg.runs
            .get(request_id)
            .cloned()
            .ok_or_else(|| RunError::not_found(format!("unknown run {request_id:?}")))
    }

    /// Handle `POST /v1/runs/<id>/cancel`.
    pub fn cancel(&self, request_id: &str) -> std::result::Result<RunCancelResponse, RunError> {
        let _ = self.require_enabled()?;
        validate_request_id(request_id)?;
        let handle = {
            let reg = lock(&self.registry);
            reg.runs
                .get(request_id)
                .cloned()
                .ok_or_else(|| RunError::not_found(format!("unknown run {request_id:?}")))?
        };
        let cancelled = handle.cancel();
        Ok(RunCancelResponse::new(request_id, cancelled))
    }

    /// Handle `GET /v1/runs/<id>/result`.
    pub fn result(&self, request_id: &str) -> std::result::Result<RunFinishResponse, RunError> {
        let _ = self.require_enabled()?;
        validate_request_id(request_id)?;
        let handle = {
            let reg = lock(&self.registry);
            reg.runs
                .get(request_id)
                .cloned()
                .ok_or_else(|| RunError::not_found(format!("unknown run {request_id:?}")))?
        };
        match handle.result() {
            Some(result) => Ok(RunFinishResponse::new(request_id, result)),
            None => Err(RunError::conflict(
                "run_in_progress",
                format!("run {request_id:?} has not finished yet"),
            )),
        }
    }

    /// Handle `GET /v1/runs/logs/<id>`.
    pub fn fetch_log(&self, request_id: &str) -> std::result::Result<RunLogResponse, RunError> {
        let volume = self.require_enabled()?;
        validate_request_id(request_id)?;
        let barge = self.open_barge()?;
        let logs_area = declare_area(&barge, volume, LOGS_AREA)?;
        let logs_root = match logs_area.resolve(LOGS_ROOT_ID) {
            Ok(root) => root,
            Err(error) if error.kind() == BargeErrorKind::NotFound => {
                return Err(RunError::not_found(format!(
                    "no log retained for run {request_id:?}"
                )));
            }
            Err(error) => {
                return Err(RunError::internal(format!(
                    "cannot open logs area: {error}"
                )))
            }
        };
        let store = LogStore::new(logs_root.path, self.log_retention_s);
        let now = self.clock.now_unix_ms();
        store
            .fetch(request_id, now)
            .map_err(|e| RunError::internal(e.to_string()))?
            .ok_or_else(|| RunError::not_found(format!("no log retained for run {request_id:?}")))
    }

    fn execute_run(
        &self,
        volume: &str,
        request: RunStartRequest,
        repo_hash: String,
        staged_patch: Option<StagedPatch>,
        handle: Arc<RunHandle>,
    ) {
        let started_ms = self.clock.now_unix_ms();
        let patch_digest = staged_patch.as_ref().map(|p| p.digest.clone());
        let platform = Platform {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        };

        let finish_with = |outcome: RunOutcome,
                           tree: String,
                           toolchain: Option<String>,
                           log: LogRef,
                           area: Option<AreaUse>| {
            let finished_ms = self.clock.now_unix_ms();
            let result = RunResult {
                machine: self.machine_name.clone(),
                platform: platform.clone(),
                repo: request.repo.clone(),
                base: request.base.clone(),
                patch_digest: patch_digest.clone(),
                tree,
                toolchain,
                command: request.command.clone(),
                started_ms,
                finished_ms,
                waking_ms: None,
                outcome,
                log,
                area,
            };
            handle.finish(finished_ms, result);
        };

        let empty_log_ref = || LogRef {
            digest: format!("sha256:{}", auth::hex(&Sha256::digest([]))),
            location: LogStore::location_for(&request.request_id),
        };

        // Open Barge areas and admit work against the registered volume's budget.
        let barge = match self.open_barge() {
            Ok(barge) => barge,
            Err(error) => {
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: error.message,
                    },
                    String::new(),
                    None,
                    empty_log_ref(),
                    None,
                );
                return;
            }
        };

        if let Err(error) = barge.admit(volume) {
            finish_with(
                RunOutcome::Infrastructure {
                    stage: RunStage::Syncing,
                    reason: format!("volume {volume:?} cannot admit run: {error}"),
                },
                String::new(),
                None,
                empty_log_ref(),
                None,
            );
            return;
        }

        let log_writer = match declare_area(&barge, volume, LOGS_AREA)
            .and_then(|area| ensure_adopted_root(&area, LOGS_ROOT_ID, "logs"))
            .and_then(|root| {
                LogStore::new(root.path, self.log_retention_s)
                    .create_writer(&request.request_id)
                    .map_err(|e| RunError::internal(e.to_string()))
            }) {
            Ok(writer) => Arc::new(Mutex::new(Some(writer))),
            Err(error) => {
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: error.message,
                    },
                    String::new(),
                    None,
                    empty_log_ref(),
                    None,
                );
                return;
            }
        };

        let finalize_log = |writer_slot: &Arc<Mutex<Option<LogWriter>>>, now: u64| -> LogRef {
            let taken = lock(writer_slot).take();
            match taken {
                Some(writer) => writer.finish(now),
                None => empty_log_ref(),
            }
        };

        // Stage 1: Syncing
        handle.push_event(
            self.clock.now_unix_ms(),
            RunStreamEventKind::Stage {
                stage: RunStage::Syncing,
            },
        );

        let mirrors_area = match declare_area(&barge, volume, MIRRORS_AREA) {
            Ok(area) => area,
            Err(error) => {
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: error.message,
                    },
                    String::new(),
                    None,
                    log,
                    None,
                );
                return;
            }
        };
        let builds_area = match declare_area(&barge, volume, BUILDS_AREA) {
            Ok(area) => area,
            Err(error) => {
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: error.message,
                    },
                    String::new(),
                    None,
                    log,
                    None,
                );
                return;
            }
        };

        let build_root_id = format!("slot-{repo_hash}-{}", request.slot);
        let build_root = match ensure_adopted_root(&builds_area, &build_root_id, "slot") {
            Ok(root) => root,
            Err(error) => {
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: error.message,
                    },
                    String::new(),
                    None,
                    log,
                    None,
                );
                return;
            }
        };

        let build_hold = match builds_area.try_hold(&build_root.id, Mode::Exclusive) {
            Ok(Some(hold)) => hold,
            Ok(None) => {
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: format!("build area root {} is currently held", build_root.id),
                    },
                    String::new(),
                    None,
                    log,
                    None,
                );
                return;
            }
            Err(error) => {
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: format!("cannot hold build area root {}: {error}", build_root.id),
                    },
                    String::new(),
                    None,
                    log,
                    None,
                );
                return;
            }
        };

        let mirror_root_id = format!("mirror-{repo_hash}");
        let mirror_root = match mirrors_area.resolve(&mirror_root_id) {
            Ok(root) => root,
            Err(error) => {
                drop(build_hold);
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: format!(
                            "mirror for repo {:?} is unavailable: {error}",
                            request.repo
                        ),
                    },
                    String::new(),
                    None,
                    log,
                    None,
                );
                return;
            }
        };

        let checkout_dir = build_root.path.join("checkout");
        let verified_tree = {
            let mirror_mutex = self.mirror_lock(&repo_hash);
            let _mirror_guard = lock(&mirror_mutex);
            git::sync_checkout(
                &mirror_root.path,
                &checkout_dir,
                &request.base,
                staged_patch.as_ref().map(|p| p.bytes.as_slice()),
                &request.expected_tree,
            )
        };
        let verified_tree = match verified_tree {
            Ok(tree) => tree,
            Err(error) => {
                drop(build_hold);
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: error.to_string(),
                    },
                    String::new(),
                    None,
                    log,
                    area_use(&builds_area, &build_root.id),
                );
                return;
            }
        };

        let run_env = match exec::build_run_env(&self.host_env, &request.env, &build_root.path) {
            Ok(env) => env,
            Err(error) => {
                drop(build_hold);
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Syncing,
                        reason: error.to_string(),
                    },
                    verified_tree,
                    None,
                    log,
                    area_use(&builds_area, &build_root.id),
                );
                return;
            }
        };

        if handle.cancelled.load(Ordering::SeqCst) {
            drop(build_hold);
            let log = finalize_log(&log_writer, self.clock.now_unix_ms());
            finish_with(
                RunOutcome::Cancelled,
                verified_tree,
                None,
                log,
                area_use(&builds_area, &build_root.id),
            );
            return;
        }

        // Stage 2: Toolchain
        handle.push_event(
            self.clock.now_unix_ms(),
            RunStreamEventKind::Stage {
                stage: RunStage::Toolchain,
            },
        );
        let toolchain = match exec::check_toolchain(&checkout_dir, &request.command, &run_env) {
            Ok(toolchain) => toolchain,
            Err(error) => {
                drop(build_hold);
                let log = finalize_log(&log_writer, self.clock.now_unix_ms());
                finish_with(
                    RunOutcome::Infrastructure {
                        stage: RunStage::Toolchain,
                        reason: error.to_string(),
                    },
                    verified_tree,
                    None,
                    log,
                    area_use(&builds_area, &build_root.id),
                );
                return;
            }
        };

        // Stage 3: Running
        handle.push_event(
            self.clock.now_unix_ms(),
            RunStreamEventKind::Stage {
                stage: RunStage::Running,
            },
        );
        let budget = Duration::from_millis(request.budget_ms);
        let clock_for_output = Arc::clone(&self.clock);
        let handle_for_output = Arc::clone(&handle);
        let writer_for_output = Arc::clone(&log_writer);
        let outcome = exec::run_command(
            &checkout_dir,
            &request.command,
            &run_env,
            budget,
            &handle.cancelled,
            move |stream: RunOutputStream, bytes: Vec<u8>| {
                if let Some(writer) = lock(&writer_for_output).as_mut() {
                    writer.append(&bytes);
                }
                handle_for_output.push_event(
                    clock_for_output.now_unix_ms(),
                    RunStreamEventKind::Output {
                        stream,
                        data_base64: base64::encode(&bytes),
                    },
                );
            },
        );

        // Drop the build hold before measuring/finishing so Barge records the
        // release activity and size cleanly.
        drop(build_hold);
        let log = finalize_log(&log_writer, self.clock.now_unix_ms());
        let area = area_use(&builds_area, &build_root.id);
        finish_with(outcome, verified_tree, toolchain, log, area);
    }
}

struct SlotGuard {
    registry: Arc<Mutex<Registry>>,
    slot_key: (String, u32),
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut reg = lock(&self.registry);
        reg.active_slots.remove(&self.slot_key);
    }
}

fn declare_area(barge: &Barge, volume: &str, name: &str) -> std::result::Result<Area, RunError> {
    barge
        .area(
            AreaSpec::new(AREA_NAMESPACE, name, volume)
                .absent(Absent::Fail)
                .counts_toward_reserve(true),
        )
        .map_err(|error| {
            RunError::internal(format!(
                "cannot declare Barge area {AREA_NAMESPACE}/{name} on volume {volume:?}: {error}"
            ))
        })
}

fn ensure_adopted_root(
    area: &Area,
    root_id: &str,
    label: &str,
) -> std::result::Result<barge::Root, RunError> {
    match area.resolve(root_id) {
        Ok(root) => return Ok(root),
        Err(error) if error.kind() == BargeErrorKind::NotFound => {}
        Err(error) => {
            return Err(RunError::internal(format!(
                "cannot resolve root {root_id:?} in area {}: {error}",
                area.key().label()
            )));
        }
    }
    let location = area.location().map_err(|error| {
        RunError::internal(format!(
            "cannot inspect area {}: {error}",
            area.key().label()
        ))
    })?;
    let Some(area_dir) = location.path() else {
        return Err(RunError::internal(format!(
            "area {} is unavailable: {location:?}",
            area.key().label()
        )));
    };
    let folder_path = area_dir.join(root_id);
    fs::create_dir_all(&folder_path).map_err(|error| {
        RunError::internal(format!(
            "cannot create root folder {}: {error}",
            folder_path.display()
        ))
    })?;
    area.adopt_root(root_id, label, root_id).map_err(|error| {
        RunError::internal(format!(
            "cannot adopt root {root_id:?} in area {}: {error}",
            area.key().label()
        ))
    })
}

fn area_use(area: &Area, root_id: &str) -> Option<AreaUse> {
    let listing = area.list().ok()?;
    let info = listing.get(root_id)?;
    Some(AreaUse {
        id: info.id.clone(),
        bytes: info.bytes,
    })
}

fn validate_request_id(value: &str) -> std::result::Result<(), RunError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(RunError::bad_request(format!(
            "invalid request_id {value:?}: expected 1..=64 ASCII letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

fn validate_repo(value: &str) -> std::result::Result<(), RunError> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(RunError::bad_request("repo must not be empty"));
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

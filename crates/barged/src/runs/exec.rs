//! Process-group command execution, strict environment construction,
//! toolchain verification, budget enforcement and cancellation.
//!
//! Tom's limits (docs/knowledge/machines.md):
//! - Command is argv only, with no shell.
//! - Environment is the explicit allowlist from the request plus `HOME`,
//!   `PATH` (with `~/.cargo/bin`) and `USER`, and executor-managed build-area
//!   paths (`CARGO_TARGET_DIR`, `TMPDIR`, `TMP`, `TEMP`) that the allowlist
//!   cannot override. Nothing else is inherited.
//! - Runs as `barged`'s own unprivileged user in its own process group.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use barge::run::{RunOutcome, RunStage};
use barge_machine::RunOutputStream;

use crate::{Error, Result};

/// Environment variables owned by the executor; callers cannot override them.
pub(crate) const RESERVED_ENV_VARS: &[&str] = &[
    "CARGO_TARGET_DIR",
    "TMPDIR",
    "TMP",
    "TEMP",
    "HOME",
    "PATH",
    "USER",
];

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERM_GRACE: Duration = Duration::from_millis(500);
const OUTPUT_CHUNK_BYTES: usize = 4096;

/// Minimal host environment passed through to remote runs (`HOME`, `PATH`, `USER`).
/// Injectable so tests never mutate process-global environment variables.
#[derive(Debug, Clone)]
pub struct HostEnv {
    pub home: PathBuf,
    pub path: String,
    pub user: String,
}

impl HostEnv {
    /// Capture `HOME`, `PATH` and `USER` from the current unprivileged user.
    pub fn from_process() -> Self {
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
        let user = std::env::var("USER").unwrap_or_default();
        Self { home, path, user }
    }

    /// Build `PATH` with `~/.cargo/bin` prepended so `rustup` honours the
    /// repository's `rust-toolchain.toml`.
    pub(crate) fn path_with_cargo_bin(&self) -> String {
        let cargo_bin = self.home.join(".cargo/bin");
        let cargo_bin_str = cargo_bin.to_string_lossy();
        if self.path.is_empty() {
            cargo_bin_str.into_owned()
        } else if self.path.split(':').any(|entry| entry == cargo_bin_str) {
            self.path.clone()
        } else {
            format!("{cargo_bin_str}:{}", self.path)
        }
    }
}

/// Validate the caller's environment allowlist before starting a run.
pub(crate) fn validate_caller_env(env: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in env {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(Error::new(format!("invalid environment key {key:?}")));
        }
        if value.contains('\0') {
            return Err(Error::new(format!(
                "environment variable {key:?} contains a NUL byte"
            )));
        }
        if RESERVED_ENV_VARS.contains(&key.as_str()) {
            return Err(Error::new(format!(
                "environment variable {key:?} is managed by barged and cannot be overridden"
            )));
        }
    }
    Ok(())
}

/// Build the sanitized environment map for a run inside `build_root`.
pub(crate) fn build_run_env(
    host: &HostEnv,
    caller_env: &BTreeMap<String, String>,
    build_root: &Path,
) -> Result<BTreeMap<String, String>> {
    validate_caller_env(caller_env)?;
    let target_dir = build_root.join("target");
    let tmp_dir = build_root.join("tmp");
    fs::create_dir_all(&target_dir).map_err(|error| {
        Error::new(format!(
            "cannot create target directory {}: {error}",
            target_dir.display()
        ))
    })?;
    fs::create_dir_all(&tmp_dir).map_err(|error| {
        Error::new(format!(
            "cannot create tmp directory {}: {error}",
            tmp_dir.display()
        ))
    })?;

    let mut env = caller_env.clone();
    env.insert("HOME".to_string(), host.home.to_string_lossy().into_owned());
    env.insert("PATH".to_string(), host.path_with_cargo_bin());
    env.insert("USER".to_string(), host.user.clone());
    env.insert(
        "CARGO_TARGET_DIR".to_string(),
        target_dir.to_string_lossy().into_owned(),
    );
    let tmp_str = tmp_dir.to_string_lossy().into_owned();
    env.insert("TMPDIR".to_string(), tmp_str.clone());
    env.insert("TMP".to_string(), tmp_str.clone());
    env.insert("TEMP".to_string(), tmp_str);
    Ok(env)
}

/// Verify the Rust toolchain when the repository declares `rust-toolchain.toml`
/// (or `rust-toolchain`) or the command invokes a Rust tool.
///
/// Missing `rustup` or a toolchain installation failure returns an error so the
/// caller reports `RunOutcome::Infrastructure { stage: RunStage::Toolchain, .. }`.
pub(crate) fn check_toolchain(
    checkout_dir: &Path,
    command: &[String],
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let has_toolchain_file = checkout_dir.join("rust-toolchain.toml").exists()
        || checkout_dir.join("rust-toolchain").exists();
    let is_rust_command = command
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "cargo" | "rustc" | "rustup" | "rustfmt" | "clippy-driver"
            )
        });
    if !has_toolchain_file && !is_rust_command {
        return Ok(None);
    }

    let mut cmd = Command::new("rustc");
    cmd.arg("--version")
        .current_dir(checkout_dir)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null());
    let output = cmd.output().map_err(|error| {
        Error::new(format!(
            "rust toolchain unavailable (ensure rustup is installed in ~/.cargo/bin): {error}"
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(Error::new(if detail.is_empty() {
            format!("rustc --version failed with status {}", output.status)
        } else {
            format!("toolchain setup failed: {detail}")
        }));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!version.is_empty()).then_some(version))
}

/// Spawn `argv` (no shell) in its own process group, stream stdout/stderr via
/// `on_output`, and enforce `budget` and `cancelled`.
pub(crate) fn run_command<F>(
    checkout_dir: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    budget: Duration,
    cancelled: &Arc<AtomicBool>,
    on_output: F,
) -> RunOutcome
where
    F: Fn(RunOutputStream, Vec<u8>) + Send + Sync + 'static,
{
    let Some((program, args)) = argv.split_first() else {
        return RunOutcome::Infrastructure {
            stage: RunStage::Running,
            reason: "command argv must not be empty".to_string(),
        };
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(checkout_dir)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // SAFETY: `setpgid(0, 0)` is async-signal-safe per POSIX.1-2008 and places
    // the child in a new process group whose pgid equals its pid so budget
    // expiry or cancellation signals the whole tree without process scanning.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunOutcome::Infrastructure {
                stage: RunStage::Running,
                reason: format!("cannot spawn {program:?}: {error}"),
            };
        }
    };

    let pgid = child.id();
    let on_output = Arc::new(on_output);
    let stdout_thread = child
        .stdout
        .take()
        .map(|pipe| spawn_pipe_reader(pipe, RunOutputStream::Stdout, Arc::clone(&on_output)));
    let stderr_thread = child
        .stderr
        .take()
        .map(|pipe| spawn_pipe_reader(pipe, RunOutputStream::Stderr, Arc::clone(&on_output)));

    let started = Instant::now();
    let outcome = loop {
        if cancelled.load(Ordering::SeqCst) {
            kill_process_group(pgid, &mut child);
            break RunOutcome::Cancelled;
        }
        if started.elapsed() >= budget {
            kill_process_group(pgid, &mut child);
            break RunOutcome::BudgetExpired;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(code) = status.code() {
                    break RunOutcome::Exited { code };
                }
                if let Some(signal) = status.signal() {
                    break RunOutcome::Signalled { signal };
                }
                break RunOutcome::Infrastructure {
                    stage: RunStage::Running,
                    reason: format!("process ended with unknown status {status}"),
                };
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                kill_process_group(pgid, &mut child);
                break RunOutcome::Infrastructure {
                    stage: RunStage::Running,
                    reason: format!("cannot wait on child process: {error}"),
                };
            }
        }
    };

    if let Some(handle) = stdout_thread {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_thread {
        let _ = handle.join();
    }
    outcome
}

fn spawn_pipe_reader<R, F>(
    mut pipe: R,
    stream: RunOutputStream,
    on_output: Arc<F>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
    F: Fn(RunOutputStream, Vec<u8>) + Send + Sync + 'static,
{
    thread::spawn(move || {
        let mut buf = [0_u8; OUTPUT_CHUNK_BYTES];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => on_output(stream, buf[..n].to_vec()),
            }
        }
    })
}

fn kill_process_group(pgid: u32, child: &mut Child) {
    let Ok(pgid_i32) = i32::try_from(pgid) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    // SAFETY: `pgid_i32` is the child's own process group created by `setpgid(0, 0)`.
    // Sending a signal to `-pgid_i32` targets only that process group.
    unsafe {
        libc::kill(-pgid_i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    // SAFETY: same process group as above; escalate to SIGKILL after grace period.
    unsafe {
        libc::kill(-pgid_i32, libc::SIGKILL);
    }
    let _ = child.wait();
}

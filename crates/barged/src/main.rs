//! `barged`: parse the config path, build the real seams, serve.
//!
//! The binary holds nothing but wiring; every mechanic is in the library so the
//! tests can replace each seam (docs/knowledge/machines.md, "Protocol and
//! trust").

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use barged::{Config, Daemon, PowerController, Result, SystemClock};

/// Cleared by the SIGTERM handler; `serve` returns once it reads false.
static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn stop_on_signal(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

/// Where a system install puts the config; `--config` overrides it.
const DEFAULT_CONFIG: &str = "/etc/barged/config.json";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("barged: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &[String]) -> Result<()> {
    let mut config_path = PathBuf::from(DEFAULT_CONFIG);
    let mut index = 0;
    let mut pair = false;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "pair" => pair = true,
            "--config" => {
                index += 1;
                let path = arguments
                    .get(index)
                    .ok_or_else(|| barged::Error::new("--config needs a path"))?;
                config_path = PathBuf::from(path);
            }
            "--help" | "-h" => {
                println!("usage: barged [pair] [--config PATH]\n\ndefault: {DEFAULT_CONFIG}");
                return Ok(());
            }
            other => {
                return Err(barged::Error::new(format!(
                    "unknown argument {other:?}; usage: barged [pair] [--config PATH]"
                )))
            }
        }
        index += 1;
    }

    let config = Config::load(&config_path)?;
    if pair {
        if config.mode != barged::AuthMode::Lan {
            return Err(barged::Error::new("pair requires LAN mode"));
        }
        let path = config
            .key_path()
            .ok_or_else(|| barged::Error::new("missing key file"))?;
        let key = barge_machine::auth::hex(
            &barge_machine::auth::random(32).map_err(|e| barged::Error::new(e.to_string()))?,
        );
        barge_machine::auth::write_key(&path, &key)
            .map_err(|e| barged::Error::new(format!("cannot write key: {e}")))?;
        println!("{key}");
        return Ok(());
    }
    if config.listen.ip().is_unspecified() {
        eprintln!("barged: warning: listening on all interfaces");
    }
    if config.mode == barged::AuthMode::Lan {
        let path = config
            .key_path()
            .ok_or_else(|| barged::Error::new("missing key file"))?;
        let key = std::fs::read_to_string(path)
            .map_err(|e| barged::Error::new(format!("cannot read LAN key: {e}")))?;
        if barge_machine::auth::decode(key.trim(), 32).is_none() {
            return Err(barged::Error::new("invalid LAN key"));
        }
    }
    let clock = Arc::new(SystemClock);
    let controller = PowerController::new(Arc::new(platform_power()), clock.clone());
    let peers = Arc::new(platform_lookup(&config));
    let daemon = Daemon::new(&config, controller.clone(), peers, clock);
    let server = daemon.bind()?;
    let countdown = controller.start().map_err(|error| {
        barged::Error::new(format!("cannot start the countdown thread: {error}"))
    })?;
    if config.mode == barged::AuthMode::Lan {
        eprintln!("barged: listening on {} in LAN mode", daemon.listen());
    } else {
        eprintln!(
            "barged: listening on {} for {} allowlisted node(s)",
            daemon.listen(),
            config.allowed_nodes.len()
        );
    }
    // SAFETY: the handler only stores to a lock-free atomic flag.
    if unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_on_signal as *const () as libc::sighandler_t,
        )
    } == libc::SIG_ERR
    {
        controller.stop();
        let _ = countdown.join();
        return Err(barged::Error::new("cannot install SIGTERM handler"));
    }
    let outcome = daemon.serve(server, &RUNNING);
    controller.stop();
    if countdown.join().is_err() {
        eprintln!("barged: the countdown thread panicked");
    }
    outcome
}

#[cfg(target_os = "linux")]
fn platform_power() -> barged::Logind {
    barged::Logind
}

#[cfg(not(target_os = "linux"))]
fn platform_power() -> barged::Unavailable {
    barged::Unavailable
}

#[cfg(unix)]
fn platform_lookup(config: &Config) -> barged::TailscaleLookup {
    barged::TailscaleLookup::new(config.tailscale_socket.clone())
}

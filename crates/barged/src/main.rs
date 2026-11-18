//! `barged`: parse the config path, build the real seams, serve.
//!
//! The binary holds nothing but wiring; every mechanic is in the library so the
//! tests can replace each seam (docs/knowledge/machines.md, "Protocol and
//! trust").

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use barged::{Config, Daemon, PowerController, Result, SystemClock};

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
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--config" => {
                index += 1;
                let path = arguments
                    .get(index)
                    .ok_or_else(|| barged::Error::new("--config needs a path"))?;
                config_path = PathBuf::from(path);
            }
            "--help" | "-h" => {
                println!("usage: barged [--config PATH]\n\ndefault: {DEFAULT_CONFIG}");
                return Ok(());
            }
            other => {
                return Err(barged::Error::new(format!(
                    "unknown argument {other:?}; usage: barged [--config PATH]"
                )))
            }
        }
        index += 1;
    }

    let config = Config::load(&config_path)?;
    let clock = Arc::new(SystemClock);
    let controller = PowerController::new(Arc::new(platform_power()), clock.clone());
    let peers = Arc::new(platform_lookup(&config));
    let daemon = Daemon::new(&config, controller.clone(), peers, clock);
    let server = daemon.bind()?;
    let countdown = controller.start().map_err(|error| {
        barged::Error::new(format!("cannot start the countdown thread: {error}"))
    })?;
    eprintln!(
        "barged: listening on {} for {} allowlisted node(s)",
        daemon.listen(),
        config.allowed_nodes.len()
    );
    let running = AtomicBool::new(true);
    let outcome = daemon.serve(server, &running);
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

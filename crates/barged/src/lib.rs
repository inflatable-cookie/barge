//! `barged`: the machine daemon. It answers `barge machine status` and performs
//! sleep, restart and shutdown for an allowlisted tailnet node.
//!
//! Three pieces sit behind seams so the daemon never needs the real thing to be
//! tested: [`PeerLookup`] resolves who called (Tailscale's local API in
//! production), [`PowerBackend`] performs the action (logind over D-Bus on
//! Linux) and [`Clock`] supplies the deadline arithmetic. Nothing here runs a
//! shell command, and the request body never names a method: [`PowerActionKind`]
//! is the whole set of things the daemon can be asked to do.

use std::fmt;

pub mod config;
pub mod peer;
pub mod power;
pub mod server;

pub use config::Config;
#[cfg(unix)]
pub use peer::TailscaleLookup;
pub use peer::{Allowlist, PeerError, PeerLookup, PeerNode};
#[cfg(target_os = "linux")]
pub use power::Logind;
#[cfg(not(target_os = "linux"))]
pub use power::Unavailable;
pub use power::{logind_method, Clock, PowerBackend, PowerController, RequestError, SystemClock};
pub use server::Daemon;

/// The daemon's error: a message naming what failed.
#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

//! `barged`: the machine daemon. It answers `barge machine status` and performs
//! sleep, restart and shutdown for allowlisted tailnet nodes or paired LAN
//! clients.
//!
//! Six pieces sit behind seams so tests never need real OS services:
//! [`PeerLookup`] resolves Tailscale callers, [`PowerBackend`] runs logind
//! actions on Linux, [`Clock`] supplies time and deadlines, [`NetworkLookup`]
//! reads interface MAC and broadcast, [`MdnsAdvertiser`] registers
//! `_barged._tcp` with Avahi on Linux, and [`PairingNotifier`] sends desktop
//! notifications with Approve/Deny actions. Nothing here runs a shell command,
//! and [`PowerActionKind`] is the whole set of things the daemon can be asked
//! to do.

use std::fmt;

pub mod config;
pub mod mdns;
pub mod net;
pub mod notify;
pub mod pairing;
pub mod peer;
pub mod power;
pub mod server;
pub mod state;
pub mod tailnet;

pub use config::{AuthMode, Config, Serving, TailnetListen, TailscaleConfig};
#[cfg(target_os = "linux")]
pub use mdns::AvahiAdvertiser;
#[cfg(not(target_os = "linux"))]
pub use mdns::UnavailableMdns;
pub use mdns::{MdnsAdvertiser, MdnsRegistration, ServiceAdvertisement};
pub use net::{InterfaceInfo, NetworkLookup, SystemNetworkLookup};
#[cfg(target_os = "linux")]
pub use notify::DesktopNotifier;
#[cfg(not(target_os = "linux"))]
pub use notify::NoopNotifier;
pub use notify::{ActionCallback, PairAction, PairNotification, PairingNotifier};
pub use pairing::{PairDecisionError, PairStartError, PairingStore, PAIR_TTL_MS};
#[cfg(unix)]
pub use peer::TailscaleLookup;
pub use peer::{Allowlist, PeerError, PeerLookup, PeerNode};
#[cfg(target_os = "linux")]
pub use power::Logind;
#[cfg(not(target_os = "linux"))]
pub use power::Unavailable;
pub use power::{logind_method, Clock, PowerBackend, PowerController, RequestError, SystemClock};
pub use server::Daemon;
pub use state::{ClientRecord, DaemonState, LEGACY_CLIENT_ID};
pub use tailnet::{TailnetSelf, TailnetStatus, TailscaleStatus, Timing};

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

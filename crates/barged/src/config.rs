//! The daemon's configuration: what it binds to, where its state lives, and
//! which callers may control it.
//!
//! `listen` is one address. A wildcard (`0.0.0.0` or `::`) is refused unless
//! `allow_wildcard` is true, the opt-in for a LAN install that listens on every
//! interface. In Tailscale mode the named tailnet address is the first half of
//! the trust model (docs/knowledge/machines.md, "Protocol and trust").

use std::ffi::CStr;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Where tailscaled's local API socket lives on a Linux install.
pub const DEFAULT_TAILSCALE_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// `barged`'s config document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The address `barged` binds: the machine's Tailscale or LAN address.
    pub listen: SocketAddr,
    /// Display name advertised over mDNS in LAN mode; defaults to the hostname.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mode: AuthMode,
    /// Directory where `barged` keeps its machine ID and paired clients list.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// Legacy shared key file; migrated into the per-client list on first start.
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    /// Permit a wildcard `listen` address, which binds every interface.
    #[serde(default)]
    pub allow_wildcard: bool,
    /// Tailscale node identities allowed to call. A node matches by stable ID
    /// (`n1234ABCD`) or by its MagicDNS name.
    #[serde(default)]
    pub allowed_nodes: Vec<String>,
    /// Tailscaled's local API socket, used for `whois`.
    #[serde(default = "default_tailscale_socket")]
    pub tailscale_socket: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    #[default]
    Tailscale,
    Lan,
}

fn default_tailscale_socket() -> PathBuf {
    PathBuf::from(DEFAULT_TAILSCALE_SOCKET)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(|error| {
            Error::new(format!("cannot read config {}: {error}", path.display()))
        })?;
        Self::parse(&bytes)
            .map_err(|error| Error::new(format!("in config {}: {error}", path.display())))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let config: Self = serde_json::from_slice(bytes)
            .map_err(|error| Error::new(format!("cannot parse config: {error}")))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.listen.ip().is_unspecified() && !self.allow_wildcard {
            return Err(Error::new(format!(
                "listen {} is a wildcard address; bind one address, or set allow_wildcard to accept every interface",
                self.listen
            )));
        }
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                return Err(Error::new("name must not be empty"));
            }
        }
        if self.mode == AuthMode::Lan {
            return Ok(());
        }
        if self.allowed_nodes.is_empty() {
            return Err(Error::new(
                "allowed_nodes is empty; list the tailnet nodes allowed to control this machine",
            ));
        }
        for node in &self.allowed_nodes {
            if node.trim().is_empty() {
                return Err(Error::new("allowed_nodes contains an empty entry"));
            }
        }
        Ok(())
    }

    pub fn key_path(&self) -> Option<PathBuf> {
        self.key_file.as_deref().map(expand_tilde)
    }

    /// The directory where `barged` persists `machine-id` and `clients.json`.
    pub fn resolved_state_dir(&self) -> PathBuf {
        if let Some(dir) = self.state_dir.as_deref() {
            return expand_tilde(dir);
        }
        if let Some(parent) = self
            .key_path()
            .and_then(|path| path.parent().map(Path::to_path_buf))
        {
            return parent;
        }
        if let Some(dir) = std::env::var_os("BARGED_STATE_DIR").filter(|v| !v.is_empty()) {
            return PathBuf::from(dir);
        }
        if let Some(dir) = std::env::var_os("STATE_DIRECTORY").filter(|v| !v.is_empty()) {
            return PathBuf::from(dir);
        }
        if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(dir).join("barged");
        }
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home).join(".local/state/barged");
        }
        PathBuf::from("/var/lib/barged")
    }

    /// Display name advertised in mDNS TXT records.
    pub fn display_name(&self) -> String {
        if let Some(name) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return name.to_string();
        }
        system_hostname().unwrap_or_else(|| "barged".to_string())
    }

    /// The allowlist this config declares.
    pub fn allowlist(&self) -> crate::Allowlist {
        crate::Allowlist::new(self.allowed_nodes.iter().cloned())
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        std::env::var_os("HOME")
            .map_or_else(|| path.to_path_buf(), |home| PathBuf::from(home).join(rest))
    } else {
        path.to_path_buf()
    }
}

fn system_hostname() -> Option<String> {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: `buf` is a live, writable buffer of `buf.len()` bytes.
    if unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    buf[buf.len() - 1] = 0;
    // SAFETY: `buf` is NUL-terminated above.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .trim()
        .to_string();
    let short = name.split('.').next().unwrap_or("").trim().to_string();
    if short.is_empty() {
        None
    } else {
        Some(short)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn document(listen: &str, nodes: &str) -> Vec<u8> {
        format!(r#"{{"listen":"{listen}","allowed_nodes":{nodes}}}"#).into_bytes()
    }

    #[test]
    fn a_valid_config_carries_its_allowlist_and_default_socket() {
        let config = Config::parse(&document("100.64.0.2:9777", r#"["macbook"]"#)).unwrap();
        assert_eq!(
            config.listen,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2)), 9777)
        );
        assert_eq!(config.mode, AuthMode::Tailscale);
        assert_eq!(config.allowed_nodes, vec!["macbook".to_string()]);
        assert_eq!(
            config.tailscale_socket,
            PathBuf::from(DEFAULT_TAILSCALE_SOCKET)
        );
        assert!(config
            .allowlist()
            .accepts(&crate::PeerNode::new("n1", "macbook")));
    }

    #[test]
    fn lan_config_needs_no_key_file_and_wildcard_requires_explicit_opt_in() {
        let minimal_lan = br#"{"listen":"192.168.1.20:9777","mode":"lan"}"#;
        assert_eq!(Config::parse(minimal_lan).unwrap().mode, AuthMode::Lan);

        let denied = br#"{"listen":"0.0.0.0:9777","mode":"lan","key_file":"/test/key"}"#;
        let allowed =
            br#"{"listen":"0.0.0.0:9777","mode":"lan","key_file":"/test/key","allow_wildcard":true}"#;
        assert!(Config::parse(denied).is_err());
        assert_eq!(Config::parse(allowed).unwrap().mode, AuthMode::Lan);
    }

    #[test]
    fn the_shipped_example_is_valid_json_this_parser_accepts() {
        // The guide says `cp packaging/barged/config.example.json …` and edit
        // the values, so the shipped file has to be a document `Config::parse`
        // takes as-is: JSON has no comments.
        let example = include_bytes!("../../../packaging/barged/config.example.json");
        let config = Config::parse(example).expect("the shipped example config parses");
        assert_eq!(config.allowed_nodes.len(), 1);
        assert!(!config.listen.ip().is_unspecified());
        assert_eq!(
            config.tailscale_socket,
            PathBuf::from(DEFAULT_TAILSCALE_SOCKET)
        );
    }

    #[test]
    fn a_wildcard_bind_an_empty_allowlist_and_stray_fields_are_refused() {
        for bytes in [
            document("0.0.0.0:9777", r#"["macbook"]"#),
            document("[::]:9777", r#"["macbook"]"#),
            document("100.64.0.2:9777", "[]"),
            document("100.64.0.2:9777", r#"[""]"#),
            br#"{"listen":"100.64.0.2:9777","allowed_nodes":["macbook"],"extra":1}"#.to_vec(),
            b"not json".to_vec(),
        ] {
            assert!(
                Config::parse(&bytes).is_err(),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }
}

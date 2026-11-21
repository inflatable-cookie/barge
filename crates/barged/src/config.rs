//! The daemon's configuration: which networks it serves, the address for each,
//! where its state lives, and which tailnet callers may control it.
//!
//! Three shapes are accepted (docs/knowledge/machines.md, "Protocol and trust"):
//!
//! - **LAN only:** `listen` is the LAN address and `mode` is `lan`.
//! - **Tailnet only:** no `mode` (or `mode` `tailscale`) and no `tailscale`
//!   block. `listen` is the tailnet address and `allowed_nodes` is required.
//!   This is the shape every existing Tailscale config has.
//! - **Both:** `listen` is the LAN address and a `tailscale` block names the
//!   tailnet listener. The recommended shape: a paired client reaches the
//!   machine on either network with the same credentials.
//!
//! `listen` is one address. A wildcard (`0.0.0.0` or `::`) is refused unless
//! `allow_wildcard` is true, the opt-in for a LAN install that listens on every
//! interface.

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
    /// The address `barged` binds on its LAN, or on the tailnet when the
    /// config names no LAN half (see [`Config::serving`]).
    pub listen: SocketAddr,
    /// Display name advertised over mDNS in LAN mode; defaults to the hostname.
    #[serde(default)]
    pub name: Option<String>,
    /// `lan` for a LAN-only daemon. Absent or `tailscale` means the tailnet
    /// shape, which is the default for existing configs.
    #[serde(default)]
    pub mode: Option<AuthMode>,
    /// Directory where `barged` keeps its machine ID and paired clients list.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// Legacy shared key file; migrated into the per-client list on first start.
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    /// Permit a wildcard `listen` address, which binds every interface.
    #[serde(default)]
    pub allow_wildcard: bool,
    /// Tailnet node identities allowed to call, in the tailnet-only shape. A
    /// node matches by stable ID (`n1234ABCD`) or by its MagicDNS name. With a
    /// `tailscale` block the list lives in that block instead.
    #[serde(default)]
    pub allowed_nodes: Vec<String>,
    /// Tailscaled's local API socket, used for `whois` and status.
    #[serde(default = "default_tailscale_socket")]
    pub tailscale_socket: PathBuf,
    /// The tailnet listener of the combined shape.
    #[serde(default)]
    pub tailscale: Option<TailscaleConfig>,
}

/// The tailnet half of a combined config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailscaleConfig {
    /// Where the tailnet listener binds: `"auto"` for this node's tailnet IPv4
    /// address on the LAN port, or an explicit `"addr:port"`.
    pub listen: TailnetListen,
    /// Tailnet nodes allowed to call without a paired client's credentials.
    /// May be empty: paired clients need no entry here.
    #[serde(default)]
    pub allowed_nodes: Vec<String>,
    /// Tailscaled's socket, when it differs from the top-level `tailscale_socket`.
    #[serde(default)]
    pub socket: Option<PathBuf>,
}

/// Where the tailnet listener binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum TailnetListen {
    /// This node's tailnet IPv4 address, asked of tailscaled, on the LAN port.
    Auto,
    /// A fixed address.
    Addr(SocketAddr),
}

impl TryFrom<String> for TailnetListen {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value.trim() == "auto" {
            return Ok(Self::Auto);
        }
        value
            .trim()
            .parse::<SocketAddr>()
            .map(Self::Addr)
            .map_err(|_| {
                format!("tailscale.listen must be \"auto\" or an address and port, not {value:?}")
            })
    }
}

impl From<TailnetListen> for String {
    fn from(value: TailnetListen) -> Self {
        match value {
            TailnetListen::Auto => "auto".to_string(),
            TailnetListen::Addr(addr) => addr.to_string(),
        }
    }
}

/// The networks a config serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serving {
    /// The LAN only: pairing, discovery and HMAC-authenticated clients.
    Lan,
    /// The tailnet only: callers identified by `whois` and the allowlist.
    Tailnet,
    /// Both: the LAN listener and the tailnet listener serve the same state.
    Both,
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

    /// The networks this config serves.
    pub fn serving(&self) -> Serving {
        if self.tailscale.is_some() {
            Serving::Both
        } else if self.mode == Some(AuthMode::Lan) {
            Serving::Lan
        } else {
            Serving::Tailnet
        }
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
        match &self.tailscale {
            Some(block) => self.validate_both(block),
            None if self.mode == Some(AuthMode::Lan) => Ok(()),
            None => {
                if self.allowed_nodes.is_empty() {
                    return Err(Error::new(
                        "allowed_nodes is empty; list the tailnet nodes allowed to control this machine",
                    ));
                }
                check_nodes(&self.allowed_nodes)
            }
        }
    }

    fn validate_both(&self, block: &TailscaleConfig) -> Result<()> {
        if self.mode == Some(AuthMode::Tailscale) {
            return Err(Error::new(
                "mode tailscale cannot be combined with a tailscale block: listen is the LAN address, so remove mode or set it to lan",
            ));
        }
        if !self.allowed_nodes.is_empty() {
            return Err(Error::new(
                "allowed_nodes cannot sit beside a tailscale block; move the list into tailscale.allowed_nodes",
            ));
        }
        if let TailnetListen::Addr(addr) = block.listen {
            if addr.ip().is_unspecified() && !self.allow_wildcard {
                return Err(Error::new(format!(
                    "tailscale.listen {addr} is a wildcard address; name the tailnet address, or set allow_wildcard"
                )));
            }
            // A paired client dials the tailnet name on the port its record
            // holds, which is the LAN port, so both listeners share it.
            if addr.port() != self.listen.port() {
                return Err(Error::new(format!(
                    "tailscale.listen {addr} must use the LAN port {}: paired clients dial one port on both networks",
                    self.listen.port()
                )));
            }
        }
        check_nodes(&block.allowed_nodes)
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

    /// The tailnet allowlist this config declares.
    pub fn allowlist(&self) -> crate::Allowlist {
        let nodes = match &self.tailscale {
            Some(block) => &block.allowed_nodes,
            None => &self.allowed_nodes,
        };
        crate::Allowlist::new(nodes.iter().cloned())
    }

    /// The socket tailscaled's local API answers on for this config.
    pub fn tailnet_socket(&self) -> PathBuf {
        self.tailscale
            .as_ref()
            .and_then(|block| block.socket.clone())
            .unwrap_or_else(|| self.tailscale_socket.clone())
    }
}

fn check_nodes(nodes: &[String]) -> Result<()> {
    for node in nodes {
        if node.trim().is_empty() {
            return Err(Error::new("allowed_nodes contains an empty entry"));
        }
    }
    Ok(())
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
        assert_eq!(config.serving(), Serving::Tailnet);
        assert_eq!(config.allowed_nodes, vec!["macbook".to_string()]);
        assert_eq!(
            config.tailscale_socket,
            PathBuf::from(DEFAULT_TAILSCALE_SOCKET)
        );
        assert_eq!(
            config.tailnet_socket(),
            PathBuf::from(DEFAULT_TAILSCALE_SOCKET)
        );
        assert!(config
            .allowlist()
            .accepts(&crate::PeerNode::new("n1", "macbook")));
    }

    #[test]
    fn lan_config_needs_no_key_file_and_wildcard_requires_explicit_opt_in() {
        let minimal_lan = br#"{"listen":"192.0.2.20:9777","mode":"lan"}"#;
        assert_eq!(Config::parse(minimal_lan).unwrap().serving(), Serving::Lan);

        let denied = br#"{"listen":"0.0.0.0:9777","mode":"lan","key_file":"/test/key"}"#;
        let allowed =
            br#"{"listen":"0.0.0.0:9777","mode":"lan","key_file":"/test/key","allow_wildcard":true}"#;
        assert!(Config::parse(denied).is_err());
        assert_eq!(Config::parse(allowed).unwrap().serving(), Serving::Lan);
    }

    #[test]
    fn the_combined_shape_serves_both_networks_and_may_have_an_empty_tailnet_list() {
        let combined = br#"{
            "listen": "192.0.2.20:9777",
            "mode": "lan",
            "tailscale": {"listen": "auto"}
        }"#;
        let config = Config::parse(combined).unwrap();
        assert_eq!(config.serving(), Serving::Both);
        assert!(config.allowlist().entries().is_empty());
        let block = config.tailscale.as_ref().unwrap();
        assert_eq!(block.listen, TailnetListen::Auto);

        // The mode may be omitted: a tailscale block names the LAN half.
        let omitted = br#"{"listen":"192.0.2.20:9777","tailscale":{"listen":"auto"}}"#;
        assert_eq!(Config::parse(omitted).unwrap().serving(), Serving::Both);

        let fixed = br#"{
            "listen": "192.0.2.20:9777",
            "mode": "lan",
            "tailscale": {"listen": "100.64.0.9:9777", "allowed_nodes": ["macbook"], "socket": "/run/ts.sock"}
        }"#;
        let config = Config::parse(fixed).unwrap();
        assert_eq!(
            config.tailscale.as_ref().unwrap().listen,
            TailnetListen::Addr(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9)),
                9777
            ))
        );
        assert!(config
            .allowlist()
            .accepts(&crate::PeerNode::new("n2", "macbook")));
        assert_eq!(config.tailnet_socket(), PathBuf::from("/run/ts.sock"));
    }

    #[test]
    fn a_combined_config_refuses_the_tailnet_mode_and_a_top_level_allowlist() {
        let tailnet_mode =
            br#"{"listen":"192.0.2.20:9777","mode":"tailscale","tailscale":{"listen":"auto"}}"#;
        assert!(Config::parse(tailnet_mode).is_err());

        let stray_list = br#"{"listen":"192.0.2.20:9777","mode":"lan","allowed_nodes":["mac"],"tailscale":{"listen":"auto"}}"#;
        assert!(Config::parse(stray_list).is_err());

        let bad_listen = br#"{"listen":"192.0.2.20:9777","tailscale":{"listen":"yesterday"}}"#;
        assert!(Config::parse(bad_listen).is_err());

        let wildcard = br#"{"listen":"192.0.2.20:9777","tailscale":{"listen":"0.0.0.0:9777"}}"#;
        assert!(Config::parse(wildcard).is_err());

        let other_port =
            br#"{"listen":"192.0.2.20:9777","tailscale":{"listen":"100.64.0.9:4000"}}"#;
        assert!(Config::parse(other_port).is_err());
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

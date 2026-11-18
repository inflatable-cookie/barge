//! The daemon's configuration: what it binds to, and which tailnet nodes may
//! ask it to do anything.
//!
//! `listen` is a single address, never a wildcard. `barged` refuses to start on
//! `0.0.0.0` or `::`, because the tailnet address is the first half of the
//! trust model (docs/knowledge/machines.md, "Protocol and trust").

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
    /// The address `barged` binds: the machine's Tailscale address in production.
    pub listen: SocketAddr,
    #[serde(default)]
    pub mode: AuthMode,
    #[serde(default)]
    pub key_file: Option<PathBuf>,
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
                "listen {} is a wildcard address; bind the machine's Tailscale address instead",
                self.listen
            )));
        }
        if self.mode == AuthMode::Lan {
            if self.key_file.is_none() {
                return Err(Error::new("LAN mode needs key_file"));
            }
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
        self.key_file.as_ref().map(|path| {
            if let Ok(rest) = path.strip_prefix("~") {
                std::env::var_os("HOME")
                    .map_or_else(|| path.clone(), |home| PathBuf::from(home).join(rest))
            } else {
                path.clone()
            }
        })
    }

    /// The allowlist this config declares.
    pub fn allowlist(&self) -> crate::Allowlist {
        crate::Allowlist::new(self.allowed_nodes.iter().cloned())
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
    fn lan_wildcard_requires_explicit_opt_in() {
        let denied = br#"{"listen":"0.0.0.0:9777","mode":"lan","key_file":"/test/key"}"#;
        let allowed = br#"{"listen":"0.0.0.0:9777","mode":"lan","key_file":"/test/key","allow_wildcard":true}"#;
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

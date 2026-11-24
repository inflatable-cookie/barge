//! `barged`: parse the config path, build the real seams, serve, or manage
//! local pairing approvals and paired clients.
//!
//! The binary holds nothing but wiring; every mechanic is in the library so the
//! tests can replace each seam (docs/knowledge/machines.md, "Protocol and
//! trust").

use std::io::{self, BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use barge_machine::DaemonClient;
use barged::{
    Clock, Config, Daemon, DaemonState, Error, NetworkLookup, PowerController, Result, Serving,
    SystemClock, SystemNetworkLookup,
};

/// Cleared by the SIGTERM handler; `serve` returns once it reads false.
static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn stop_on_signal(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

/// Where a system install puts the config; `--config` overrides it.
const DEFAULT_CONFIG: &str = "/etc/barged/config.json";

const USAGE: &str = "\
usage:
  barged [--config PATH]
  barged pair --approve [--config PATH] [--request ID] [--deny] [--yes]
  barged clients list [--config PATH]
  barged clients remove <client-id> [--config PATH]";

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
    match arguments.first().map(String::as_str) {
        Some("pair") => return run_pair(&arguments[1..]),
        Some("clients") => return run_clients(&arguments[1..]),
        Some("--help" | "-h") => {
            println!("{USAGE}\n\ndefault config: {DEFAULT_CONFIG}");
            return Ok(());
        }
        _ => {}
    }

    let config_path = parse_config_only(arguments)?;
    let config = Config::load(&config_path)?;
    if config.listen.ip().is_unspecified() {
        eprintln!("barged: warning: listening on all interfaces");
    }
    let clock = Arc::new(SystemClock);
    let controller = PowerController::new(Arc::new(platform_power()), clock.clone());
    let peers = Arc::new(platform_lookup(&config));
    let network = Arc::new(SystemNetworkLookup);
    let daemon =
        Daemon::new(&config, controller.clone(), peers, clock)?.with_network(network.clone());
    let listeners = daemon.bind_all()?;
    let bound_addr = listeners
        .first()
        .and_then(|l| l.local_addr().ok())
        .unwrap_or_else(|| daemon.listen());
    if let Some(warning) = interface_warning(&*network, bound_addr.ip()) {
        eprintln!("{warning}");
    }
    let countdown = controller
        .start()
        .map_err(|error| Error::new(format!("cannot start the countdown thread: {error}")))?;
    match config.serving() {
        Serving::Lan => eprintln!(
            "barged: listening on {} in LAN mode (id {})",
            bound_addr,
            daemon.state_store().machine_id()
        ),
        Serving::Both => eprintln!(
            "barged: listening on {} for LAN clients (id {}); the tailnet listener follows when tailscale is up",
            bound_addr,
            daemon.state_store().machine_id()
        ),
        Serving::Tailnet => eprintln!(
            "barged: listening on {} for {} allowlisted node(s)",
            bound_addr,
            config.allowed_nodes.len()
        ),
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
        return Err(Error::new("cannot install SIGTERM handler"));
    }
    let outcome = daemon.serve_all(listeners, &RUNNING);
    controller.stop();
    if countdown.join().is_err() {
        eprintln!("barged: the countdown thread panicked");
    }
    outcome
}

fn interface_warning(lookup: &dyn NetworkLookup, address: IpAddr) -> Option<String> {
    if address.is_loopback() || address.is_unspecified() {
        return None;
    }

    let info = lookup.interface_for(address);
    let missing = [
        (info.mac.is_none(), "MAC"),
        (info.broadcast.is_none(), "broadcast"),
    ]
    .into_iter()
    .filter_map(|(is_missing, label)| is_missing.then_some(label))
    .collect::<Vec<_>>();

    if missing.is_empty() {
        return None;
    }

    let mut warning = format!(
        "barged: warning: cannot determine {} for bound address {address}",
        missing.join(" or ")
    );
    if let Some(error) = info.lookup_error {
        warning.push_str(&format!(": {error}"));
    }
    warning.push_str("; a sandboxed unit needs AF_NETLINK to read interface details");
    Some(warning)
}

fn parse_config_only(arguments: &[String]) -> Result<PathBuf> {
    let mut config_path = PathBuf::from(DEFAULT_CONFIG);
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--config" => {
                index += 1;
                let path = arguments
                    .get(index)
                    .ok_or_else(|| Error::new("--config needs a path"))?;
                config_path = PathBuf::from(path);
            }
            other => {
                return Err(Error::new(format!("unknown argument {other:?}\n\n{USAGE}")));
            }
        }
        index += 1;
    }
    Ok(config_path)
}

fn run_pair(arguments: &[String]) -> Result<()> {
    let mut config_path = PathBuf::from(DEFAULT_CONFIG);
    let mut approve = false;
    let mut deny = false;
    let mut yes = false;
    let mut request_id: Option<String> = None;
    let mut url: Option<String> = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--approve" => approve = true,
            "--deny" => deny = true,
            "--yes" => yes = true,
            "--config" => {
                index += 1;
                let path = arguments
                    .get(index)
                    .ok_or_else(|| Error::new("--config needs a path"))?;
                config_path = PathBuf::from(path);
            }
            "--request" => {
                index += 1;
                let id = arguments
                    .get(index)
                    .ok_or_else(|| Error::new("--request needs a request ID"))?;
                request_id = Some(id.clone());
            }
            "--url" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| Error::new("--url needs a daemon URL"))?;
                url = Some(value.clone());
            }
            other => {
                return Err(Error::new(format!(
                    "unknown pair option {other:?}; usage: barged pair --approve [--config PATH] [--request ID] [--deny] [--yes]"
                )));
            }
        }
        index += 1;
    }
    if !approve {
        return Err(Error::new(
            "pairing no longer prints a key; start pairing from a client and run `barged pair --approve` to approve it",
        ));
    }
    let base = match url {
        Some(base) => base,
        None => {
            let config = Config::load(&config_path)?;
            if config.serving() == Serving::Tailnet {
                return Err(Error::new(
                    "pair requires a LAN address: set mode to lan, or add a tailscale block beside a LAN listen",
                ));
            }
            local_daemon_url(config.listen)?
        }
    };
    let client = DaemonClient::at(base);
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    run_pair_approve(
        &client,
        request_id.as_deref(),
        deny,
        yes,
        &mut stdin,
        &mut stdout,
    )
}

fn local_daemon_url(listen: SocketAddr) -> Result<String> {
    if listen.port() == 0 {
        return Err(Error::new(
            "config listen port is 0; pass --url http://127.0.0.1:<port>",
        ));
    }
    let addr = match listen.ip() {
        IpAddr::V6(_) if listen.ip().is_loopback() || listen.ip().is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), listen.port())
        }
        _ => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listen.port()),
    };
    Ok(format!("http://{addr}"))
}

fn run_pair_approve(
    client: &DaemonClient,
    request_id: Option<&str>,
    deny: bool,
    yes: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let pending = client
        .pending_pairs()
        .map_err(|e| Error::new(e.message()))?;
    if pending.requests.is_empty() {
        if let Some(id) = request_id {
            return Err(Error::new(format!("no pending pairing request {id:?}")));
        }
        writeln!(output, "no pending pairing requests")
            .map_err(|e| Error::new(format!("cannot write output: {e}")))?;
        return Ok(());
    }

    for (idx, item) in pending.requests.iter().enumerate() {
        writeln!(
            output,
            "[{}] {}  code: {}  id: {}",
            idx + 1,
            item.client_name,
            item.code,
            item.request_id
        )
        .map_err(|e| Error::new(format!("cannot write output: {e}")))?;
    }

    let chosen = if let Some(id) = request_id {
        pending
            .requests
            .iter()
            .find(|item| item.request_id == id)
            .ok_or_else(|| Error::new(format!("no pending pairing request {id:?}")))?
            .clone()
    } else if yes && pending.requests.len() == 1 {
        pending.requests[0].clone()
    } else {
        let prompt = if deny {
            "Deny request [1-N or ID, empty to cancel]: "
        } else {
            "Approve request [1-N or ID, empty to cancel]: "
        };
        write!(output, "{prompt}").map_err(|e| Error::new(format!("cannot write prompt: {e}")))?;
        output
            .flush()
            .map_err(|e| Error::new(format!("cannot flush prompt: {e}")))?;
        let mut line = String::new();
        input
            .read_line(&mut line)
            .map_err(|e| Error::new(format!("cannot read answer: {e}")))?;
        let answer = line.trim();
        if answer.is_empty() {
            return Err(Error::new("pairing approval cancelled"));
        }
        if let Ok(num) = answer.parse::<usize>() {
            pending
                .requests
                .get(num.saturating_sub(1))
                .filter(|_| num >= 1)
                .cloned()
                .ok_or_else(|| Error::new(format!("no request #{num}")))?
        } else {
            pending
                .requests
                .iter()
                .find(|item| item.request_id == answer)
                .cloned()
                .ok_or_else(|| Error::new(format!("no pending pairing request {answer:?}")))?
        }
    };

    if deny {
        client
            .deny_pair(&chosen.request_id)
            .map_err(|e| Error::new(e.message()))?;
        writeln!(
            output,
            "denied {} ({})",
            chosen.client_name, chosen.request_id
        )
        .map_err(|e| Error::new(format!("cannot write output: {e}")))?;
    } else {
        let approved = client
            .approve_pair(&chosen.request_id)
            .map_err(|e| Error::new(e.message()))?;
        writeln!(
            output,
            "approved {} (client id {})",
            approved.client.name, approved.client.id
        )
        .map_err(|e| Error::new(format!("cannot write output: {e}")))?;
    }
    Ok(())
}

fn run_clients(arguments: &[String]) -> Result<()> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err(Error::new(
            "usage: barged clients list [--config PATH] | barged clients remove <id> [--config PATH]",
        ));
    };
    let rest = &arguments[1..];
    match subcommand {
        "list" => {
            let config_path = parse_config_only(rest)?;
            let config = Config::load(&config_path)?;
            let state = open_state(&config)?;
            let clients = state.list_clients()?;
            if clients.is_empty() {
                println!("no paired clients");
            } else {
                for client in clients {
                    println!(
                        "{}: {} (paired {})",
                        client.id, client.name, client.paired_at_unix_ms
                    );
                }
            }
            Ok(())
        }
        "remove" => {
            let mut config_path = PathBuf::from(DEFAULT_CONFIG);
            let mut target: Option<String> = None;
            let mut index = 0;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--config" => {
                        index += 1;
                        let path = rest
                            .get(index)
                            .ok_or_else(|| Error::new("--config needs a path"))?;
                        config_path = PathBuf::from(path);
                    }
                    other if other.starts_with("--") => {
                        return Err(Error::new(format!("unknown option {other:?}")));
                    }
                    id => {
                        if target.is_some() {
                            return Err(Error::new("clients remove takes one <client-id>"));
                        }
                        target = Some(id.to_string());
                    }
                }
                index += 1;
            }
            let id = target
                .ok_or_else(|| Error::new("usage: barged clients remove <id> [--config PATH]"))?;
            let config = Config::load(&config_path)?;
            let state = open_state(&config)?;
            if !state.remove_client(&id)? {
                return Err(Error::new(format!("client {id:?} is not paired")));
            }
            println!("removed client {id}");
            Ok(())
        }
        other => Err(Error::new(format!(
            "unknown clients subcommand {other:?}; use list or remove"
        ))),
    }
}

fn open_state(config: &Config) -> Result<DaemonState> {
    DaemonState::open(
        config.resolved_state_dir(),
        config.key_path().as_deref(),
        SystemClock.now_unix_ms(),
    )
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
    barged::TailscaleLookup::new(config.tailnet_socket())
}

#[cfg(test)]
mod tests {
    use super::*;
    use barged::InterfaceInfo;
    use std::io::Cursor;
    use std::thread;

    struct FixedNetworkLookup(InterfaceInfo);

    impl NetworkLookup for FixedNetworkLookup {
        fn interface_for(&self, _ip: IpAddr) -> InterfaceInfo {
            self.0.clone()
        }
    }

    #[test]
    fn startup_warning_names_missing_metadata_and_lookup_error() {
        let lookup = FixedNetworkLookup(InterfaceInfo {
            lookup_error: Some("Permission denied (os error 13)".to_string()),
            ..InterfaceInfo::default()
        });

        let warning = interface_warning(&lookup, "192.0.2.10".parse().unwrap()).unwrap();
        assert!(warning.contains("192.0.2.10"), "{warning}");
        assert!(warning.contains("MAC or broadcast"), "{warning}");
        assert!(
            warning.contains("Permission denied (os error 13)"),
            "{warning}"
        );
        assert!(warning.contains("AF_NETLINK"), "{warning}");
    }

    #[test]
    fn startup_warning_names_missing_mac_without_an_os_error() {
        let lookup = FixedNetworkLookup(InterfaceInfo {
            broadcast: Some(Ipv4Addr::BROADCAST),
            ..InterfaceInfo::default()
        });

        let warning = interface_warning(&lookup, "192.0.2.10".parse().unwrap()).unwrap();
        assert!(warning.contains("cannot determine MAC"), "{warning}");
        assert!(!warning.contains("os error"), "{warning}");
    }

    #[test]
    fn startup_warning_skips_loopback_and_complete_interface() {
        let lookup = FixedNetworkLookup(InterfaceInfo {
            mac: Some("02:00:00:00:00:01".to_string()),
            broadcast: Some(Ipv4Addr::BROADCAST),
            ..InterfaceInfo::default()
        });

        assert_eq!(
            interface_warning(&lookup, IpAddr::V4(Ipv4Addr::LOCALHOST)),
            None
        );
        assert_eq!(
            interface_warning(&lookup, "192.0.2.10".parse().unwrap()),
            None
        );
    }

    #[test]
    fn pair_approve_lists_codes_and_approves_selected_request() {
        let temp = barge::testing::TempDir::new("barged-cli-approve").unwrap();
        let config = Config::parse(
            format!(
                r#"{{"listen":"127.0.0.1:0","mode":"lan","state_dir":{}}}"#,
                serde_json::to_string(temp.path()).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
        let clock = Arc::new(SystemClock);
        let controller = PowerController::new(Arc::new(platform_power()), clock.clone());
        let daemon = Arc::new(
            Daemon::new(
                &config,
                controller,
                Arc::new(barged::TailscaleLookup::new("/nonexistent")),
                clock,
            )
            .unwrap(),
        );
        let listener = daemon.bind().unwrap();
        let addr = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let server = thread::spawn(move || daemon.serve(listener, &flag).unwrap());

        let client = DaemonClient::at(format!("http://{addr}"));
        let keys = barge_machine::KeyPair::generate().unwrap();
        let started = client
            .start_pair("MacBook", &keys.public_key_hex())
            .unwrap();
        let peer_pub = barge_machine::parse_public_key(&started.public_key).unwrap();
        let expected_code = keys
            .derive(&peer_pub, barge_machine::PairRole::Client)
            .unwrap()
            .code()
            .to_string();

        let mut output = Vec::new();
        let mut input = Cursor::new(b"1\n");
        run_pair_approve(&client, None, false, false, &mut input, &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("MacBook"), "{text}");
        assert!(text.contains(&expected_code), "{text}");
        assert!(text.contains("approved MacBook"), "{text}");

        let status = client.pair_status(&started.request_id).unwrap();
        assert_eq!(status.status, barge_machine::PairStatus::Approved);
        assert!(status.client_id.is_some());

        running.store(false, Ordering::SeqCst);
        server.join().unwrap();
    }
}

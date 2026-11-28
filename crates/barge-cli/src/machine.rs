use std::io::Write;
use std::time::{Duration, Instant};

use barge::{Barge, Error, ErrorKind, Result};
use barge_machine::{
    backfill, follow_addresses, local_hostname, match_registered_with, parse_public_key,
    remember_path, sanitize_machine_name, AssumeYes, Confirmation, DaemonClient, DatagramSender,
    DiscoveredMachine, KeyPair, Machine, MachineLink, MatchProbe, MdnsBrowser, NetworkProbe,
    PairRole, PairStatus, Path, PowerActionKind, PowerState, Registry, State, UdpMdnsBrowser,
    UdpSender, DEFAULT_DISCOVER_TIMEOUT, DEFAULT_POWER_DELAY_S,
};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::human;
use crate::output::document;

const VERBS: &str = "discover, add, list, remove, wake, pair, unpair, clients, revoke, status, hold, release, sleep, restart, shutdown or cancel";
const PAIR_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_PAIR_TIMEOUT: Duration = Duration::from_secs(125);

/// Where a machine verb reaches the outside world: the registry, Wake-on-LAN
/// sender, and mDNS browser. Tests replace each one.
pub(crate) struct Env<'a, S, B> {
    pub(crate) discover: &'a dyn Fn() -> Result<Registry>,
    pub(crate) sender: &'a S,
    pub(crate) browser: &'a B,
    pub(crate) probe: &'a dyn MatchProbe,
    pub(crate) pair_poll_interval: Duration,
}

pub(crate) fn machine(arguments: &[String], json: bool) -> Result<()> {
    let discover = || Registry::discover().map_err(machine_error);
    machine_dispatch(
        arguments,
        json,
        &Env {
            discover: &discover,
            sender: &UdpSender,
            browser: &UdpMdnsBrowser,
            probe: &NetworkProbe,
            pair_poll_interval: PAIR_POLL_INTERVAL,
        },
    )
}

/// Answers nothing: the tests never reach the network.
#[cfg(test)]
struct SilentProbe;

#[cfg(test)]
impl MatchProbe for SilentProbe {
    fn mac(&self, _found: &DiscoveredMachine) -> Option<String> {
        None
    }

    fn resolve(&self, _hostname: &str) -> Vec<std::net::IpAddr> {
        Vec::new()
    }
}

#[cfg(test)]
pub(crate) fn machine_with<S: DatagramSender, B: MdnsBrowser>(
    arguments: &[String],
    json: bool,
    registry: &Registry,
    sender: &S,
    browser: &B,
) -> Result<()> {
    let discover = || Ok(registry.clone());
    machine_dispatch(
        arguments,
        json,
        &Env {
            discover: &discover,
            sender,
            browser,
            probe: &SilentProbe,
            pair_poll_interval: Duration::from_millis(10),
        },
    )
}

fn machine_dispatch<S: DatagramSender, B: MdnsBrowser>(
    arguments: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(Error::usage(format!("machine needs {VERBS}")));
    };
    let rest = &arguments[1..];
    match command {
        "discover" => discover_machines(rest, json, env),
        "add" => add(rest, json, env),
        "list" => list(rest, json, env),
        "remove" => remove(rest, json, env),
        "wake" => wake(rest, json, env),
        "pair" => pair(rest, json, env),
        "unpair" => unpair(rest, json, env),
        "clients" => clients(rest, json, env),
        "revoke" => revoke(rest, json, env),
        "status" => status(rest, json, env),
        "hold" => hold(rest, json),
        "release" => release(rest, json),
        "sleep" => machine_power(PowerActionKind::Sleep, rest, json, env),
        "restart" => machine_power(PowerActionKind::Restart, rest, json, env),
        "shutdown" => machine_power(PowerActionKind::Shutdown, rest, json, env),
        "cancel" => cancel(rest, json, env),
        other => Err(Error::usage(format!(
            "unknown machine command {other:?}; use {VERBS}"
        ))),
    }
}

fn hold(rest: &[String], json: bool) -> Result<()> {
    let options = Options::parse(rest, &["reason"], &[])?;
    let machine = options.single("machine hold", "id")?;
    let reason = options.get("reason");
    Barge::open()?.machine_hold(&machine, reason.clone())?;
    if json {
        document(
            "barge.machine-held/1",
            vec![("machine_id", json!(machine)), ("reason", json!(reason))],
        )?;
    } else {
        println!("held machine {machine} for new runs");
    }
    Ok(())
}

fn release(rest: &[String], json: bool) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let machine = options.single("machine release", "id")?;
    Barge::open()?.machine_release(&machine)?;
    if json {
        document(
            "barge.machine-released/1",
            vec![("machine_id", json!(machine))],
        )?;
    } else {
        println!("released machine hold for {machine}");
    }
    Ok(())
}

fn discover_machines<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &["timeout"], &[])?;
    expect_no_positionals(&options)?;
    let timeout = parse_timeout(&options, DEFAULT_DISCOVER_TIMEOUT)?;
    let registry = (env.discover)()?;
    let found = env.browser.browse(timeout).map_err(machine_error)?;

    // Keep registered IP/port current when a paired machine's DHCP lease moves.
    let _ = follow_addresses(&registry, &found);
    let registered = registry.list().map_err(machine_error)?;
    let matches: Vec<Option<&Machine>> = found
        .iter()
        .map(|item| match_registered_with(item, &registered, env.probe))
        .collect();

    let entries: Vec<serde_json::Value> = found
        .iter()
        .zip(&matches)
        .map(|(item, matched)| {
            json!({
                "id": item.id,
                "name": item.name,
                "mode": item.mode,
                "protocol_version": item.protocol_version,
                "address": item.address,
                "port": item.port,
                "registered_as": matched.map(Machine::name),
            })
        })
        .collect();

    if json {
        document(
            "barge.machine-discover/1",
            vec![("discovered", serde_json::Value::Array(entries))],
        )?;
    } else if found.is_empty() {
        println!("no machines found");
    } else {
        for (item, matched) in found.iter().zip(&matches) {
            let status = match matched {
                Some(m) => format!("registered as {}", m.name()),
                None => "not registered".to_string(),
            };
            println!(
                "{} ({}) at {}:{} [{}] — {}",
                item.name, item.id, item.address, item.port, item.mode, status
            );
        }
    }
    Ok(())
}

fn add<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(
        rest,
        &[
            "name",
            "hostname",
            "tailscale-name",
            "mac",
            "broadcast",
            "port",
            "daemon-port",
            "id",
            "timeout",
        ],
        &[],
    )?;
    let manual = options.get("hostname").is_some()
        || options.get("mac").is_some()
        || options.get("tailscale-name").is_some()
        || options.get("broadcast").is_some()
        || options.get("port").is_some()
        || options.get("daemon-port").is_some();

    let machine = if manual {
        expect_no_positionals(&options)?;
        if options.get("id").is_some() || options.get("timeout").is_some() {
            return Err(Error::usage(
                "--id and --timeout cannot be combined with manual --hostname/--mac flags",
            ));
        }
        machine_from_options(&options)?
    } else if let Some(target) = discovery_target(&options)? {
        let timeout = parse_timeout(&options, DEFAULT_DISCOVER_TIMEOUT)?;
        let found = env.browser.browse(timeout).map_err(machine_error)?;
        let chosen = select_discovered(&found, &target)?;
        let local_name = options
            .get("name")
            .unwrap_or_else(|| sanitize_machine_name(&chosen.name));
        Machine::discovered(
            chosen.id.clone(),
            local_name,
            chosen.address.clone(),
            chosen.port,
        )
        .map_err(machine_error)?
    } else {
        // Preserves the existing required-flag usage error when `machine add`
        // is invoked with no arguments.
        machine_from_options(&options)?
    };

    let registry = (env.discover)()?;
    registry.add(machine.clone()).map_err(machine_error)?;
    if json {
        document("barge.machine/1", vec![("machine", json!(machine))])?;
    } else {
        println!(
            "registered machine {} ({})",
            machine.name(),
            machine.hostname()
        );
    }
    Ok(())
}

fn discovery_target(options: &Options) -> Result<Option<String>> {
    match (options.get("id"), options.positionals.as_slice()) {
        (Some(_), [_, ..]) => Err(Error::usage(
            "machine add takes either <id-or-name> or --id, not both",
        )),
        (Some(id), []) => Ok(Some(id)),
        (None, [target]) => Ok(Some(target.clone())),
        (None, []) => Ok(None),
        (None, _) => Err(Error::usage(
            "machine add takes at most one <id-or-name> positional argument",
        )),
    }
}

fn select_discovered<'a>(
    found: &'a [DiscoveredMachine],
    target: &str,
) -> Result<&'a DiscoveredMachine> {
    if let Some(exact_id) = found.iter().find(|item| item.id == target) {
        return Ok(exact_id);
    }
    let matches: Vec<&DiscoveredMachine> = found
        .iter()
        .filter(|item| item.name.eq_ignore_ascii_case(target))
        .collect();
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => Err(Error::not_found(format!(
            "no discovered machine matches {target:?}"
        ))),
        _ => Err(Error::refused(format!(
            "multiple discovered machines match {target:?}; pass the machine ID"
        ))),
    }
}

/// Record what a registered machine reported where its record lacks it. A
/// different machine ID is logged and the record is left alone.
fn record_reported(registry: &Registry, name: &str, state: &State) {
    if let Ok(outcome) = backfill(registry, name, state) {
        if let Some(reported) = outcome.id_mismatch {
            eprintln!(
                "barge: {name} reports machine ID {reported}, not the ID it is registered with; the record is unchanged"
            );
        }
    }
}

fn machine_from_options(options: &Options) -> Result<Machine> {
    let name = options.required("name")?;
    let hostname = options.required("hostname")?;
    let mac = options.required("mac")?;
    let mut machine = Machine::new(name, hostname, &mac).map_err(machine_error)?;
    if let Some(name) = options.get("tailscale-name") {
        machine = machine.with_tailscale_name(Some(name));
    }
    if let Some(address) = options.get("broadcast") {
        machine = machine.with_broadcast(
            address
                .parse()
                .map_err(|_| Error::usage("--broadcast must be an IPv4 address"))?,
        );
    }
    if let Some(port) = options.get("port") {
        let port = port
            .parse::<u16>()
            .map_err(|_| Error::usage("--port must be between 1 and 65535"))?;
        machine = machine.with_port(port).map_err(machine_error)?;
    }
    if let Some(port) = options.get("daemon-port") {
        let port = port
            .parse::<u16>()
            .map_err(|_| Error::usage("--daemon-port must be between 1 and 65535"))?;
        machine = machine.with_daemon_port(port).map_err(machine_error)?;
    }
    Ok(machine)
}

fn list<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    expect_no_positionals(&options)?;
    let registry = (env.discover)()?;
    let machines = registry.list().map_err(machine_error)?;
    if json {
        document("barge.machine-list/1", vec![("machines", json!(machines))])?;
    } else if machines.is_empty() {
        println!("no machines registered");
    } else {
        for machine in machines {
            println!(
                "{}: {} ({})",
                machine.name(),
                machine.hostname(),
                machine.mac().unwrap_or("unpaired")
            );
        }
    }
    Ok(())
}

fn remove<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine remove", "name")?;
    let registry = (env.discover)()?;
    let removed = registry.remove(&name).map_err(machine_error)?;
    if json {
        document("barge.machine-removed/1", vec![("removed", json!(removed))])?;
    } else {
        println!("removed machine {}", removed.name());
    }
    Ok(())
}

fn wake<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine wake", "name")?;
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let (destination, bytes_sent) =
        barge_machine::wake(&machine, env.sender).map_err(machine_error)?;
    if json {
        document(
            "barge.machine-wake/1",
            vec![
                ("machine_name", json!(machine.name())),
                ("destination", json!(destination.to_string())),
                ("bytes_sent", json!(bytes_sent)),
            ],
        )?;
    } else {
        println!(
            "sent Wake-on-LAN packet for {} to {}",
            machine.name(),
            destination
        );
    }
    Ok(())
}

fn pair<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &["client-name", "timeout"], &[])?;
    let name = options.single("machine pair", "name")?;
    let timeout = parse_timeout(&options, DEFAULT_PAIR_TIMEOUT)?;
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;

    let client_name = options
        .get("client-name")
        .or_else(local_hostname)
        .unwrap_or_else(|| "barge-cli".to_string());
    let keys = KeyPair::generate().map_err(machine_error)?;
    let unauth = DaemonClient::for_lan_unauthenticated(&machine);
    let started = unauth
        .start_pair(&client_name, &keys.public_key_hex())
        .map_err(machine_error)?;
    let daemon_pub = parse_public_key(&started.public_key).map_err(machine_error)?;
    let derived = keys
        .derive(&daemon_pub, PairRole::Client)
        .map_err(machine_error)?;

    if json {
        eprintln!("pairing code for {}: {}", machine.name(), derived.code());
    } else {
        println!("pairing code for {}: {}", machine.name(), derived.code());
        let _ = std::io::stdout().flush();
    }

    let deadline = Instant::now() + timeout;
    let client_id = loop {
        let status = unauth
            .pair_status(&started.request_id)
            .map_err(machine_error)?;
        match status.status {
            PairStatus::Pending => {
                if Instant::now() >= deadline {
                    return Err(Error::refused(format!(
                        "pairing request for {} timed out",
                        machine.name()
                    )));
                }
                std::thread::sleep(env.pair_poll_interval);
            }
            PairStatus::Approved => {
                let id = status.client_id.ok_or_else(|| {
                    Error::new(
                        ErrorKind::UnknownState,
                        "daemon approved pairing without a client_id",
                    )
                })?;
                break id;
            }
            PairStatus::Denied => {
                return Err(Error::refused(format!(
                    "pairing request for {} was denied",
                    machine.name()
                )));
            }
            PairStatus::Expired => {
                return Err(Error::refused(format!(
                    "pairing request for {} expired",
                    machine.name()
                )));
            }
        }
    };

    registry
        .pair_client(&name, &client_id, derived.key())
        .map_err(machine_error)?;

    let authed =
        DaemonClient::for_lan_client(&machine, Some(client_id.clone()), derived.key().to_vec());
    if let Ok(state) = authed.state() {
        let _ = registry.update(&name, |record| {
            let mut next = record.clone();
            if !state.machine_id.trim().is_empty() {
                next = next.with_id(Some(state.machine_id.clone()));
            }
            if let Some(mac) = &state.mac {
                next = next.with_mac(mac)?;
            }
            if let Some(broadcast) = state.broadcast {
                next = next.with_broadcast(broadcast);
            }
            *record = next;
            Ok(())
        });
    }

    if json {
        document(
            "barge.machine-paired/1",
            vec![
                ("machine_name", json!(name)),
                ("client_id", json!(client_id)),
            ],
        )?;
    } else {
        println!("paired {name} (client {client_id})");
    }
    Ok(())
}

fn unpair<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine unpair", "name")?;
    let registry = (env.discover)()?;
    registry.unpair(&name).map_err(machine_error)?;
    if json {
        document(
            "barge.machine-unpaired/1",
            vec![("machine_name", json!(name))],
        )?;
    } else {
        println!("unpaired {name}");
    }
    Ok(())
}

fn clients<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine clients", "name")?;
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let client = lan_client(&registry, &machine)?;
    let list = client.clients().map_err(machine_error)?;
    if json {
        document(
            "barge.machine-clients/1",
            vec![
                ("machine_name", json!(machine.name())),
                ("clients", json!(list.clients)),
            ],
        )?;
    } else if list.clients.is_empty() {
        println!("no paired clients on {}", machine.name());
    } else {
        for item in list.clients {
            println!(
                "{}: {} (paired {} local)",
                item.id,
                item.name,
                human::local_clock(item.paired_at_unix_ms)
            );
        }
    }
    Ok(())
}

fn revoke<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let (machine_name, client_id) = match options.positionals.as_slice() {
        [machine, client] => (machine.clone(), client.clone()),
        _ => return Err(Error::usage("machine revoke needs <name> <client-id>")),
    };
    let registry = (env.discover)()?;
    let machine = registry.get(&machine_name).map_err(machine_error)?;
    let local_creds = registry
        .credentials(machine.name())
        .map_err(machine_error)?;
    let client = lan_client(&registry, &machine)?;
    let revoked = client.revoke_client(&client_id).map_err(machine_error)?;

    let revoked_self = local_creds.as_ref().is_some_and(|creds| {
        creds.client_id.as_deref() == Some(revoked.client_id.as_str())
            || (creds.client_id.is_none() && revoked.client_id == "legacy")
    });
    if revoked_self {
        let _ = registry.unpair(machine.name());
    }

    if json {
        document(
            "barge.machine-client-revoked/1",
            vec![
                ("machine_name", json!(machine.name())),
                ("client_id", json!(revoked.client_id)),
            ],
        )?;
    } else {
        println!("revoked client {} on {}", revoked.client_id, machine.name());
    }
    Ok(())
}

fn status<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine status", "name")?;
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let reached = machine_link(&registry, &machine)?
        .run(|client| client.state())
        .map_err(machine_error)?;
    let (state, mode) = (reached.value, reached.path.as_str());
    record_reported(&registry, machine.name(), &state);
    remember_route(&registry, machine.name(), reached.path)?;
    if json {
        document(
            "barge.machine-status/1",
            vec![
                ("machine_name", json!(machine.name())),
                ("machine_state", json!(state)),
                ("mode", json!(mode)),
            ],
        )?;
    } else {
        let now = human::now_unix_ms();
        println!(
            "{}: {}, {} (observed {} local, {mode})",
            machine.name(),
            power_name(state.power),
            human::age_text(now, state.observed_at_unix_ms),
            human::local_clock(state.observed_at_unix_ms),
        );
        if let Some(pending) = state.pending {
            println!(
                "  {} pending until {} local ({})",
                pending.action.as_str(),
                human::local_clock(pending.deadline_unix_ms),
                human::until_text(now, pending.deadline_unix_ms),
            );
        }
    }
    Ok(())
}

fn cancel<S: DatagramSender, B: MdnsBrowser>(
    rest: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine cancel", "name")?;
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let reached = machine_link(&registry, &machine)?
        .run(|client| client.cancel())
        .map_err(machine_error)?;
    remember_route(&registry, machine.name(), reached.path)?;
    let result = reached.value;
    if json {
        document(
            "barge.machine-cancel/1",
            vec![
                ("machine_name", json!(machine.name())),
                ("cancelled", json!(result.cancelled)),
            ],
        )?;
    } else if result.cancelled {
        println!("cancelled the pending action on {}", machine.name());
    } else {
        println!("nothing was pending on {}", machine.name());
    }
    Ok(())
}

/// The routes to a machine's daemon: its LAN address, then its tailnet name,
/// signed with the stored credentials. Only the route that answered is recorded.
fn machine_link(registry: &Registry, machine: &Machine) -> Result<MachineLink> {
    let credentials = registry
        .credentials(machine.name())
        .map_err(machine_error)?;
    Ok(MachineLink::for_machine(machine, credentials))
}

/// The LAN route to a machine's daemon, for the verbs the daemon serves only on
/// the LAN: listing and revoking paired clients.
fn lan_client(registry: &Registry, machine: &Machine) -> Result<DaemonClient> {
    Ok(
        match registry
            .credentials(machine.name())
            .map_err(machine_error)?
        {
            Some(creds) => DaemonClient::for_lan_client(machine, creds.client_id, creds.key),
            None => DaemonClient::for_lan_unauthenticated(machine),
        },
    )
}

/// Remember the route that reached a machine, so it is tried first next time.
fn remember_route(registry: &Registry, name: &str, path: Path) -> Result<()> {
    remember_path(registry, name, path).map_err(machine_error)
}

/// `machine sleep|restart|shutdown <name> [--delay SECONDS] [--yes]`.
///
/// Sleep starts its countdown at once; restart and shutdown ask first unless
/// `--yes` was given (docs/knowledge/machines.md, "Behaviour").
fn machine_power<S: DatagramSender, B: MdnsBrowser>(
    action: PowerActionKind,
    arguments: &[String],
    json: bool,
    env: &Env<'_, S, B>,
) -> Result<()> {
    let options = Options::parse(arguments, &["delay"], &["yes"])?;
    let name = options.single(&format!("machine {}", action.as_str()), "name")?;
    let delay = match options.get("delay") {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| Error::usage("--delay must be whole seconds"))?,
        None => DEFAULT_POWER_DELAY_S,
    };
    if json
        && !options.flag("yes")
        && matches!(action, PowerActionKind::Restart | PowerActionKind::Shutdown)
    {
        return Err(Error::usage(
            "--json requires --yes for restart or shutdown",
        ));
    }
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let link = machine_link(&registry, &machine)?;
    let reached = if options.flag("yes") {
        link.request_power(machine.name(), action, delay, &AssumeYes)
    } else {
        link.request_power(machine.name(), action, delay, &Prompt)
    }
    .map_err(machine_error)?;
    remember_route(&registry, machine.name(), reached.path)?;
    let result = reached.value;
    if json {
        document(
            "barge.machine-power/1",
            vec![
                ("machine_name", json!(machine.name())),
                ("action", json!(result.action.as_str())),
                ("deadline_unix_ms", json!(result.deadline_unix_ms)),
            ],
        )?;
    } else {
        println!(
            "{} of {} will fire at {} local (in {}s); cancel with `barge machine cancel {}`",
            result.action.as_str(),
            machine.name(),
            human::local_clock(result.deadline_unix_ms),
            delay,
            machine.name()
        );
    }
    Ok(())
}

fn parse_timeout(options: &Options, default: Duration) -> Result<Duration> {
    let Some(raw) = options.get("timeout") else {
        return Ok(default);
    };
    let seconds: f64 = raw
        .parse()
        .map_err(|_| Error::usage("--timeout must be non-negative seconds"))?;
    if !seconds.is_finite() || !(0.0..=3600.0).contains(&seconds) {
        return Err(Error::usage("--timeout must be 0-3600 seconds"));
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// The interactive confirmation: one line from stdin, anything but yes is no.
struct Prompt;

impl Confirmation for Prompt {
    fn confirm(&self, action: PowerActionKind, machine: &str, delay_s: u32) -> bool {
        eprint!("{} {} in {delay_s}s? [y/N] ", action.as_str(), machine);
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

pub(crate) fn power_name(power: PowerState) -> &'static str {
    match power {
        PowerState::Awake => "awake",
        PowerState::Asleep => "asleep",
        PowerState::Unknown => "unknown",
    }
}

pub(crate) fn machine_error(error: barge_machine::Error) -> Error {
    let kind = match error.kind() {
        barge_machine::ErrorKind::Usage => ErrorKind::Usage,
        barge_machine::ErrorKind::Refused => ErrorKind::Refused,
        barge_machine::ErrorKind::NotFound => ErrorKind::NotFound,
        barge_machine::ErrorKind::Io => ErrorKind::Io,
        barge_machine::ErrorKind::UnknownState => ErrorKind::UnknownState,
        // An absent daemon is not a state the storage library knows: the
        // message says "unreachable" and the CLI exits 3.
        barge_machine::ErrorKind::Unreachable => ErrorKind::Io,
    };
    Error::new(kind, error.message())
}

#[cfg(test)]
mod tests {
    use super::machine_with;
    use barge::testing::TempDir;
    use barge_machine::{
        parse_public_key, DatagramSender, DiscoveredMachine, KeyPair, MdnsBrowser, PairResponse,
        PairRole, PairStatus, PairStatusResponse, Registry,
    };
    use std::io::{self, Read, Write};

    fn read_http_request(stream: &mut impl Read) -> io::Result<Vec<u8>> {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte)?;
            request.push(byte[0]);
        }
        let headers = String::from_utf8_lossy(&request);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = request.len();
        request.resize(body_start + length, 0);
        stream.read_exact(&mut request[body_start..])?;
        Ok(request)
    }
    use std::net::{SocketAddr, TcpListener};
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeSender(Mutex<Vec<(Vec<u8>, SocketAddr)>>);

    impl DatagramSender for FakeSender {
        fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
            self.0.lock().unwrap().push((bytes.to_vec(), destination));
            Ok(bytes.len())
        }
    }

    struct FakeBrowser(Vec<DiscoveredMachine>);

    impl MdnsBrowser for FakeBrowser {
        fn browse(&self, _timeout: Duration) -> barge_machine::Result<Vec<DiscoveredMachine>> {
            Ok(self.0.clone())
        }
    }

    fn arguments(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn machine_cli_add_list_and_wake_use_the_fake_sender() {
        // The fixture removes itself, even if an assertion below panics.
        let temp = TempDir::new("cli-machine").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        let sender = FakeSender::default();
        let browser = FakeBrowser(Vec::new());
        for words in [
            &[
                "add",
                "--name",
                "pc",
                "--hostname",
                "pc.local",
                "--mac",
                "00:11:22:33:44:ff",
                "--broadcast",
                "192.0.2.255",
            ][..],
            &["list"][..],
            &["wake", "pc"][..],
        ] {
            machine_with(&arguments(words), true, &registry, &sender, &browser).unwrap();
        }
        let sent = sender.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0.len(), 102);
        assert_eq!(sent[0].1, "192.0.2.255:9".parse().unwrap());
        assert_eq!(registry.list().unwrap().len(), 1);
    }

    #[test]
    fn machine_discover_and_add_from_discovery_need_no_host_or_mac() {
        let temp = TempDir::new("cli-machine-discover").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        let sender = FakeSender::default();
        let browser = FakeBrowser(vec![DiscoveredMachine {
            id: "mid-1234".to_string(),
            name: "Linux PC".to_string(),
            mode: "lan".to_string(),
            protocol_version: "1".to_string(),
            address: "192.0.2.42".to_string(),
            port: 9777,
            host: "linux-pc.local".to_string(),
        }]);
        machine_with(
            &arguments(&["discover"]),
            false,
            &registry,
            &sender,
            &browser,
        )
        .unwrap();
        machine_with(
            &arguments(&["add", "Linux PC"]),
            false,
            &registry,
            &sender,
            &browser,
        )
        .unwrap();
        let saved = registry.get("linux-pc").unwrap();
        assert_eq!(saved.id(), Some("mid-1234"));
        assert_eq!(saved.hostname(), "192.0.2.42");
        assert_eq!(saved.daemon_port(), 9777);
        assert_eq!(saved.mac(), None);

        // DHCP renewal updates registered IP when discover sees the same machine ID.
        let moved = FakeBrowser(vec![DiscoveredMachine {
            id: "mid-1234".to_string(),
            name: "Linux PC".to_string(),
            mode: "lan".to_string(),
            protocol_version: "1".to_string(),
            address: "192.0.2.99".to_string(),
            port: 9777,
            host: "linux-pc.local".to_string(),
        }]);
        machine_with(&arguments(&["discover"]), true, &registry, &sender, &moved).unwrap();
        assert_eq!(registry.get("linux-pc").unwrap().hostname(), "192.0.2.99");
    }

    #[test]
    fn machine_pair_runs_x25519_flow_and_records_state_mac_and_broadcast() {
        let temp = TempDir::new("cli-machine-pair-flow").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        let sender = FakeSender::default();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = FakeBrowser(vec![DiscoveredMachine {
            id: "mid-9".to_string(),
            name: "pc".to_string(),
            mode: "lan".to_string(),
            protocol_version: "1".to_string(),
            address: "127.0.0.1".to_string(),
            port,
            host: "pc.local".to_string(),
        }]);

        let daemon_keys = KeyPair::generate().unwrap();
        let daemon_pub = daemon_keys.public_key_hex();
        let server = std::thread::spawn(move || {
            let mut derived_key = None;
            for step in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream).unwrap();
                let text = String::from_utf8_lossy(&request);
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
                let (status, reply) = if text.starts_with("POST /v1/pair ") {
                    let req: barge_machine::PairRequest = serde_json::from_str(body).unwrap();
                    let client_pub = parse_public_key(&req.public_key).unwrap();
                    derived_key = Some(
                        daemon_keys
                            .derive(&client_pub, PairRole::Daemon)
                            .unwrap()
                            .key()
                            .to_vec(),
                    );
                    (
                        202,
                        serde_json::to_string(&PairResponse::new("req-1", &daemon_pub, 9_999_999))
                            .unwrap(),
                    )
                } else if step == 1 && text.starts_with("GET /v1/pair/req-1 ") {
                    (
                        200,
                        serde_json::to_string(&PairStatusResponse::new(
                            "req-1",
                            PairStatus::Pending,
                            None,
                        ))
                        .unwrap(),
                    )
                } else if text.starts_with("GET /v1/pair/req-1 ") {
                    (
                        200,
                        serde_json::to_string(&PairStatusResponse::new(
                            "req-1",
                            PairStatus::Approved,
                            Some("cid-42".to_string()),
                        ))
                        .unwrap(),
                    )
                } else {
                    assert!(text.contains("x-barge-client-id: cid-42"), "{text}");
                    (
                        200,
                        r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42,"machine_id":"mid-9","mac":"aa:bb:cc:dd:ee:ff","broadcast":"192.0.2.255"}"#.to_string(),
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
            derived_key.unwrap()
        });

        machine_with(
            &arguments(&["add", "pc"]),
            false,
            &registry,
            &sender,
            &browser,
        )
        .unwrap();
        machine_with(
            &arguments(&["pair", "pc", "--client-name", "MacBook"]),
            false,
            &registry,
            &sender,
            &browser,
        )
        .unwrap();

        let expected_key = server.join().unwrap();
        let creds = registry.credentials("pc").unwrap().unwrap();
        assert_eq!(creds.client_id.as_deref(), Some("cid-42"));
        assert_eq!(creds.key, expected_key);

        let updated = registry.get("pc").unwrap();
        assert_eq!(updated.mac(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(updated.broadcast(), std::net::Ipv4Addr::new(192, 0, 2, 255));
    }
}

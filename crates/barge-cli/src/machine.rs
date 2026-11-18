use std::io::Write;

use barge::{Error, ErrorKind, Result};
use barge_machine::{
    request_power, AssumeYes, Confirmation, DaemonClient, DatagramSender, Machine, PowerActionKind,
    PowerState, Registry, UdpSender, DEFAULT_POWER_DELAY_S,
};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::output::document;

pub(crate) fn machine(arguments: &[String], json: bool) -> Result<()> {
    machine_dispatch(
        arguments,
        json,
        &|| Registry::discover().map_err(machine_error),
        &UdpSender,
    )
}

#[cfg(test)]
pub(crate) fn machine_with(
    arguments: &[String],
    json: bool,
    registry: &Registry,
    sender: &impl DatagramSender,
) -> Result<()> {
    machine_dispatch(arguments, json, &|| Ok(registry.clone()), sender)
}

fn machine_dispatch(
    arguments: &[String],
    json: bool,
    discover: &impl Fn() -> Result<Registry>,
    sender: &impl DatagramSender,
) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(Error::usage("machine needs add, list, remove, wake, pair, unpair, status, sleep, restart, shutdown or cancel"));
    };
    let rest = &arguments[1..];
    match command {
        "add" => {
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
                ],
                &[],
            )?;
            expect_no_positionals(&options)?;
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
            let registry = discover()?;
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
        "list" => {
            let options = Options::parse(rest, &[], &[])?;
            expect_no_positionals(&options)?;
            let registry = discover()?;
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
                        machine.mac()
                    );
                }
            }
            Ok(())
        }
        "remove" => {
            let options = Options::parse(rest, &[], &[])?;
            let name = options.single("machine remove", "name")?;
            let registry = discover()?;
            let removed = registry.remove(&name).map_err(machine_error)?;
            if json {
                document("barge.machine-removed/1", vec![("removed", json!(removed))])?;
            } else {
                println!("removed machine {}", removed.name());
            }
            Ok(())
        }
        "wake" => {
            let options = Options::parse(rest, &[], &[])?;
            let name = options.single("machine wake", "name")?;
            let registry = discover()?;
            let machine = registry.get(&name).map_err(machine_error)?;
            let (destination, bytes_sent) = barge_machine::wake(&machine, sender).map_err(machine_error)?;
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
        "pair" => {
            let options = Options::parse(rest, &[], &[])?;
            let name = options.single("machine pair", "name")?;
            let mut key = String::new();
            std::io::stdin().read_line(&mut key).map_err(|e| Error::new(ErrorKind::Io, format!("cannot read key: {e}")))?;
            let registry = discover()?;
            registry.pair(&name, &key).map_err(machine_error)?;
            if json { document("barge.machine-paired/1", vec![("machine_name", json!(name))])?; } else { println!("paired {name}"); }
            Ok(())
        }
        "unpair" => {
            let options = Options::parse(rest, &[], &[])?;
            let name = options.single("machine unpair", "name")?;
            let registry = discover()?;
            registry.unpair(&name).map_err(machine_error)?;
            if json { document("barge.machine-unpaired/1", vec![("machine_name", json!(name))])?; } else { println!("unpaired {name}"); }
            Ok(())
        }
        "status" => {
            let options = Options::parse(rest, &[], &[])?;
            let name = options.single("machine status", "name")?;
            let registry = discover()?;
            let machine = registry.get(&name).map_err(machine_error)?;
            let (client, mode) = machine_client(&registry, &machine)?;
            let state = client.state().map_err(machine_error)?;
            if json {
                document(
                    "barge.machine-status/1",
                    vec![("machine_name", json!(machine.name())), ("machine_state", json!(state)), ("mode", json!(mode))],
                )?;
            } else {
                println!(
                    "{}: {}, observed at {} ({mode})",
                    machine.name(),
                    power_name(state.power),
                    state.observed_at_unix_ms
                );
                if let Some(pending) = state.pending {
                    println!(
                        "  {} pending until {}",
                        pending.action.as_str(),
                        pending.deadline_unix_ms
                    );
                }
            }
            Ok(())
        }
        "sleep" => machine_power(
            PowerActionKind::Sleep,
            rest,
            json,
            discover,
        ),
        "restart" => machine_power(
            PowerActionKind::Restart,
            rest,
            json,
            discover,
        ),
        "shutdown" => machine_power(
            PowerActionKind::Shutdown,
            rest,
            json,
            discover,
        ),
        "cancel" => {
            let options = Options::parse(rest, &[], &[])?;
            let name = options.single("machine cancel", "name")?;
            let registry = discover()?;
            let machine = registry.get(&name).map_err(machine_error)?;
            let result = machine_client(&registry, &machine)?.0
                .cancel()
                .map_err(machine_error)?;
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
        other => Err(Error::usage(format!(
            "unknown machine command {other:?}; use add, list, remove, wake, pair, unpair, status, sleep, restart, shutdown or cancel"
        ))),
    }
}

fn machine_client(registry: &Registry, machine: &Machine) -> Result<(DaemonClient, &'static str)> {
    if let Some(key) = registry.key(machine.name()).map_err(machine_error)? {
        Ok((DaemonClient::for_lan(machine, key), "lan"))
    } else {
        Ok((DaemonClient::for_machine(machine), "tailscale"))
    }
}

/// `machine sleep|restart|shutdown <name> [--delay SECONDS] [--yes]`.
///
/// Sleep starts its countdown at once; restart and shutdown ask first unless
/// `--yes` was given (docs/knowledge/machines.md, "Behaviour").
pub(crate) fn machine_power(
    action: PowerActionKind,
    arguments: &[String],
    json: bool,
    discover: &impl Fn() -> Result<Registry>,
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
    let registry = discover()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let (client, _) = machine_client(&registry, &machine)?;
    let result = if options.flag("yes") {
        request_power(&client, machine.name(), action, delay, &AssumeYes)
    } else {
        request_power(&client, machine.name(), action, delay, &Prompt)
    }
    .map_err(machine_error)?;
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
            "{} of {} will fire at {} (in {}s); cancel with `barge machine cancel {}`",
            result.action.as_str(),
            machine.name(),
            result.deadline_unix_ms,
            delay,
            machine.name()
        );
    }
    Ok(())
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
    use barge_machine::{DatagramSender, Registry};
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeSender(Mutex<Vec<(Vec<u8>, SocketAddr)>>);

    impl DatagramSender for FakeSender {
        fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
            self.0.lock().unwrap().push((bytes.to_vec(), destination));
            Ok(bytes.len())
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
                "192.168.1.255",
            ][..],
            &["list"][..],
            &["wake", "pc"][..],
        ] {
            machine_with(&arguments(words), true, &registry, &sender).unwrap();
        }
        let sent = sender.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0.len(), 102);
        assert_eq!(sent[0].1, "192.168.1.255:9".parse().unwrap());
        assert_eq!(registry.list().unwrap().len(), 1);
    }
}

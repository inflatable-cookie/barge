use std::io::Write;

use barge::{Error, ErrorKind, Result};
use barge_machine::{
    request_power, AssumeYes, Confirmation, DaemonClient, DatagramSender, Machine, PowerActionKind,
    PowerState, Registry, UdpSender, DEFAULT_POWER_DELAY_S,
};
use serde_json::json;

use crate::args::{expect_no_positionals, Options};
use crate::console::{read_secret, StdinConsole};
use crate::human;
use crate::output::document;

const VERBS: &str =
    "add, list, remove, wake, pair, unpair, status, sleep, restart, shutdown or cancel";

/// Where a machine verb reaches the outside world: the registry, the Wake-on-LAN
/// sender and the key read by `machine pair`. Tests replace each one.
pub(crate) struct Env<'a, S> {
    pub(crate) discover: &'a dyn Fn() -> Result<Registry>,
    pub(crate) sender: &'a S,
    pub(crate) keys: &'a dyn KeyInput,
}

/// The LAN key a `machine pair` call stores: one line, with echo off on a terminal.
pub(crate) trait KeyInput {
    fn read_key(&self) -> Result<String>;
}

/// Standard input, read through the console seam.
struct StdinKey;

impl KeyInput for StdinKey {
    fn read_key(&self) -> Result<String> {
        read_secret(&mut std::io::stdin().lock(), &StdinConsole)
            .map_err(|e| Error::usage(format!("cannot read key from stdin: {e}")))
    }
}

pub(crate) fn machine(arguments: &[String], json: bool) -> Result<()> {
    let discover = || Registry::discover().map_err(machine_error);
    machine_dispatch(
        arguments,
        json,
        &Env {
            discover: &discover,
            sender: &UdpSender,
            keys: &StdinKey,
        },
    )
}

#[cfg(test)]
pub(crate) fn machine_with<S: DatagramSender>(
    arguments: &[String],
    json: bool,
    registry: &Registry,
    sender: &S,
    keys: &dyn KeyInput,
) -> Result<()> {
    let discover = || Ok(registry.clone());
    machine_dispatch(
        arguments,
        json,
        &Env {
            discover: &discover,
            sender,
            keys,
        },
    )
}

fn machine_dispatch<S: DatagramSender>(
    arguments: &[String],
    json: bool,
    env: &Env<'_, S>,
) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(Error::usage(format!("machine needs {VERBS}")));
    };
    let rest = &arguments[1..];
    match command {
        "add" => add(rest, json, env),
        "list" => list(rest, json, env),
        "remove" => remove(rest, json, env),
        "wake" => wake(rest, json, env),
        "pair" => pair(rest, json, env),
        "unpair" => unpair(rest, json, env),
        "status" => status(rest, json, env),
        "sleep" => machine_power(PowerActionKind::Sleep, rest, json, env),
        "restart" => machine_power(PowerActionKind::Restart, rest, json, env),
        "shutdown" => machine_power(PowerActionKind::Shutdown, rest, json, env),
        "cancel" => cancel(rest, json, env),
        other => Err(Error::usage(format!(
            "unknown machine command {other:?}; use {VERBS}"
        ))),
    }
}

fn add<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
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
    let machine = machine_from_options(&options)?;
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

fn list<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
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
                machine.mac()
            );
        }
    }
    Ok(())
}

fn remove<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
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

fn wake<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
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

fn pair<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine pair", "name")?;
    let key = env.keys.read_key()?;
    let registry = (env.discover)()?;
    registry.pair(&name, &key).map_err(machine_error)?;
    if json {
        document(
            "barge.machine-paired/1",
            vec![("machine_name", json!(name))],
        )?;
    } else {
        println!("paired {name}");
    }
    Ok(())
}

fn unpair<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
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

fn status<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine status", "name")?;
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let (client, mode) = machine_client(&registry, &machine)?;
    let state = client.state().map_err(machine_error)?;
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

fn cancel<S: DatagramSender>(rest: &[String], json: bool, env: &Env<'_, S>) -> Result<()> {
    let options = Options::parse(rest, &[], &[])?;
    let name = options.single("machine cancel", "name")?;
    let registry = (env.discover)()?;
    let machine = registry.get(&name).map_err(machine_error)?;
    let (client, _) = machine_client(&registry, &machine)?;
    let result = client.cancel().map_err(machine_error)?;
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
fn machine_power<S: DatagramSender>(
    action: PowerActionKind,
    arguments: &[String],
    json: bool,
    env: &Env<'_, S>,
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
    use super::{machine_with, KeyInput};
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

    /// Stands in for the key on standard input.
    struct FixedKey(&'static str);

    impl KeyInput for FixedKey {
        fn read_key(&self) -> barge::Result<String> {
            Ok(self.0.to_string())
        }
    }

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

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
            machine_with(&arguments(words), true, &registry, &sender, &FixedKey(KEY)).unwrap();
        }
        let sent = sender.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0.len(), 102);
        assert_eq!(sent[0].1, "192.168.1.255:9".parse().unwrap());
        assert_eq!(registry.list().unwrap().len(), 1);
    }

    #[test]
    fn machine_pair_stores_the_key_read_from_the_seam() {
        let temp = TempDir::new("cli-machine-pair").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        let sender = FakeSender::default();
        let add = arguments(&[
            "add",
            "--name",
            "pc",
            "--hostname",
            "pc.local",
            "--mac",
            "00:11:22:33:44:ff",
        ]);
        machine_with(&add, false, &registry, &sender, &FixedKey(KEY)).unwrap();
        machine_with(
            &arguments(&["pair", "pc"]),
            false,
            &registry,
            &sender,
            &FixedKey(KEY),
        )
        .unwrap();
        assert!(registry.key("pc").unwrap().is_some());
    }
}

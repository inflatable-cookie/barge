//! Barge's CLI: argument parsing, the JSON envelope, exit codes and the human
//! summary. Every mechanic lives in the `barge` library.
//!
//! Exit codes: 0 success, 1 refused (nothing changed, including a name that does
//! not exist), 2 usage error, 3 internal error. `--json` prints one document with a top-level `schema`
//! field, `barge.<kind>/<version>`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use barge::{
    Absent, ApplyOptions, AreaSpec, Availability, Barge, Binding, BindingState, BlockState, Class,
    DoctorReport, Durability, Error, ErrorKind, InitAction, InitCheck, InitReport,
    MoveApplyOptions, Placement, Result, RootInfo, VolumeRegistration, VolumeState, VolumeStatus,
};
use barge_machine::{
    request_power, AssumeYes, Confirmation, DaemonClient, DatagramSender, Machine, PowerActionKind,
    PowerState, Registry, UdpSender, DEFAULT_POWER_DELAY_S,
};
use serde_json::{json, Value};

mod machine;
use machine::*;
mod layout;
use layout::*;
mod survey;
use survey::*;
mod bind;
use bind::*;
mod move_resource;
use move_resource::*;
mod doctor;
use doctor::*;
mod init;
use init::*;
mod volume;
use volume::*;
mod area;
use area::*;
mod root;
use root::*;
mod prune;
use prune::*;
mod scratch;
use scratch::*;
mod human;
use human::*;
mod args;
use args::*;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let json = arguments
        .first()
        .is_some_and(|argument| argument == "--json");
    let arguments = if json {
        &arguments[1..]
    } else {
        &arguments[..]
    };
    match run(arguments, json) {
        Ok(code) => code,
        Err(error) => {
            report_error(&error, json);
            ExitCode::from(match error.kind() {
                ErrorKind::Refused | ErrorKind::NotFound => 1,
                ErrorKind::Usage => 2,
                _ => 3,
            })
        }
    }
}

fn run(arguments: &[String], json: bool) -> Result<ExitCode> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(Error::usage(usage_text()));
    };
    let rest = &arguments[1..];
    match command {
        "help" | "--help" | "-h" => {
            println!("{}", usage_text());
            Ok(ExitCode::SUCCESS)
        }
        "doctor" => doctor(rest, json),
        "init" => init(rest, json),
        other => {
            dispatch(other, rest, json)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn dispatch(command: &str, rest: &[String], json: bool) -> Result<()> {
    match command {
        "status" => status(rest, json),
        "layout" => layout(rest, json),
        "path" => resource_path(rest, json),
        "env" if json => Err(Error::usage("env does not support --json")),
        "env" => resource_env(rest),
        "survey" => survey(rest, json),
        "volume" => volume(rest, json),
        "area" => area(rest, json),
        "root" => root(rest, json),
        "bind" => bind(rest, json),
        "move" => move_resource(rest, json),
        "prune" => prune(rest, json),
        "scratch" => scratch(rest, json),
        "machine" => machine(rest, json),
        other => Err(Error::usage(format!(
            "unknown command {other:?}\n\n{}",
            usage_text()
        ))),
    }
}

fn usage_text() -> String {
    "\
barge — storage volumes, areas, roots, holds, pins, pruning and scratch

usage: barge [--json] <command>

  layout show|set                          inspect or change placements
  path <class> [--project P]              resolve a resource path
  env [--shell zsh|bash|fish]             shell assignments
  survey <volume> [--no-bytes]            inspect top-level items
  status                                  every volume, area and free space
  volume register --name N --mount PATH   register a volume (reads its UUID)
    [--uuid U] [--root-dir DIR] [--availability permanent|ephemeral]
    [--durability expendable|kept] [--reserve SIZE] [--floor SIZE] [--import-layout]
  volume list                             registered volumes and their state
  volume status <name>                    one volume's identity and space
  area declare --namespace NS --name A --volume V
    [--folder DIR] [--absent fail|fallback:PATH] [--soft-cap SIZE]
    [--counts-toward-reserve true|false]
  area list [--volume V]                  declared areas and their location
  root list <namespace>/<area> [--bytes]   roots, holds, pins and stragglers
  root pin <namespace>/<area> <id> [--reason TEXT]
  root unpin <namespace>/<area> <id>
  root evict <namespace>/<area> <id>      rename into trash, then delete
  bind list                               recorded bindings
  bind plan --consumer C --class K --link PATH --to DEST
                                          the exact before and after; print it
  bind apply --plan FILE|- [--replace]    apply exactly that reviewed plan
  bind adopt --consumer C --class K --link PATH
  bind undo <consumer>                    restore what was there before
  move <class> <new-path> --plan           review a class move
  move --item PATH --into CLASS[/SUB] --plan
  move --apply --plan FILE|- [--confirm-stopped] [--retarget-unrecorded]
  doctor                                  volumes, layout and binding drift
  init [--project P] [--path DIR] [--check]
                                          write the AGENTS.md scratch block;
                                          --check reports it and writes nothing
  prune --volume V [--dry-run]            evict idle roots to meet policy
  scratch new --volume V --project P --label L [--ttl SECONDS]
  scratch sweep --volume V [--project P]  delete expired idle scratch roots
  machine add --name N --hostname H --mac MAC [--tailscale-name N]
    [--broadcast IPv4] [--port PORT] [--daemon-port PORT]
  machine list | remove <name> | wake <name>
  machine pair <name> | unpair <name>
  machine status <name>                   what barged on that machine reports
  machine sleep <name> [--delay SECONDS]  start the countdown at once
  machine restart <name> [--delay S] [--yes]
  machine shutdown <name> [--delay S] [--yes]
  machine cancel <name>                   stop a pending countdown

SIZES: integer bytes, or KiB, MiB, GiB, TiB.
"
    .to_string()
}

fn status(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &[], &[])?;
    expect_no_positionals(&options)?;
    let barge = Barge::open_read_only()?;
    let status = barge.status()?;
    if json {
        document("barge.status/1", vec![("volumes", json!(status))])?;
    } else {
        for volume in &status {
            println!("{}", volume_line(volume));
            for area in &volume.areas {
                println!("  {}", area_line(area));
            }
        }
    }
    Ok(())
}

fn document(schema: &str, fields: Vec<(&str, Value)>) -> Result<()> {
    let mut data = serde_json::Map::new();
    for (name, value) in fields {
        data.insert(name.to_string(), value);
    }
    let document = json!({ "schema": schema, "data": Value::Object(data) });
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{document}")
        .map_err(|source| Error::new(ErrorKind::Io, format!("cannot write to stdout: {source}")))?;
    Ok(())
}

fn report_error(error: &Error, json: bool) {
    let mut stderr = std::io::stderr().lock();
    if json {
        let document = json!({
            "schema": "barge.error/1",
            "error": { "kind": error.kind().as_str(), "message": error.message() },
        });
        let _ = writeln!(stderr, "{document}");
    } else {
        let _ = writeln!(stderr, "barge: {}", error.message());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shell_assignments_quote_paths() {
        let path = std::path::Path::new("/some path/it's here");
        assert_eq!(
            super::shell_assignment("bash", "BARGE_SCRATCH", path).unwrap(),
            "export BARGE_SCRATCH='/some path/it'\\''s here'"
        );
        assert_eq!(
            super::shell_assignment("fish", "BARGE_SCRATCH", path).unwrap(),
            "set -gx BARGE_SCRATCH '/some path/it\\'s here';"
        );
    }

    use super::*;
    use barge::testing::TempDir;
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

    #[test]
    fn machine_cli_add_list_and_wake_use_the_fake_sender() {
        // The fixture removes itself, even if an assertion below panics.
        let temp = TempDir::new("cli-machine").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        let sender = FakeSender::default();
        let add = [
            "add",
            "--name",
            "pc",
            "--hostname",
            "pc.local",
            "--mac",
            "00:11:22:33:44:ff",
            "--broadcast",
            "192.168.1.255",
        ];
        machine_with(
            &add.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            true,
            &registry,
            &sender,
        )
        .unwrap();
        let list = ["list"];
        machine_with(
            &list.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            true,
            &registry,
            &sender,
        )
        .unwrap();
        let wake = ["wake", "pc"];
        machine_with(
            &wake.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            true,
            &registry,
            &sender,
        )
        .unwrap();
        let sent = sender.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0.len(), 102);
        assert_eq!(sent[0].1, "192.168.1.255:9".parse().unwrap());
        assert_eq!(registry.list().unwrap().len(), 1);
    }

    #[test]
    fn sizes_parse_and_refuse() {
        assert_eq!(parse_size("10GiB").unwrap(), 10 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1 MiB").unwrap(), 1024 * 1024);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("1PiB").is_err());
    }

    #[test]
    fn options_refuse_unknown_flags() {
        let parsed = Options::parse(
            &["--volume".to_string(), "Builds".to_string()],
            &["volume"],
            &[],
        )
        .unwrap();
        assert_eq!(parsed.get("volume").as_deref(), Some("Builds"));
        assert!(Options::parse(&["--nope".to_string()], &[], &[]).is_err());
    }

    #[test]
    fn absent_policies_parse() {
        assert_eq!(parse_absent("fail").unwrap(), Absent::Fail);
        assert_eq!(
            parse_absent("fallback:/tmp/pool").unwrap(),
            Absent::Fallback {
                path: PathBuf::from("/tmp/pool")
            }
        );
        assert!(parse_absent("fallback:relative").is_err());
    }
}

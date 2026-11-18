//! Read-only information about the machine running Barge.

use serde::Serialize;

#[derive(Serialize)]
pub struct LocalMachine {
    pub name: String,
    pub model: Option<String>,
    pub uptime_seconds: Option<u64>,
    pub volumes: Vec<crate::storage::VolumeSummary>,
    pub doctor_findings: Option<usize>,
    pub storage_error: Option<String>,
}

fn system_value(command: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(command)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

pub fn local_machine() -> LocalMachine {
    let name = system_value("hostname", &[]).unwrap_or_else(|| "This Mac".into());
    let model = system_value("sysctl", &["-n", "hw.model"]);
    let uptime_seconds = system_value("sysctl", &["-n", "kern.boottime"])
        .and_then(|value| {
            value
                .split("sec = ")
                .nth(1)?
                .split(',')
                .next()?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .and_then(|boot| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs()
                .checked_sub(boot)
        });
    let storage = (|| {
        let account = barge::Barge::open_read_only().map_err(|error| error.to_string())?;
        let volumes = crate::storage::volumes(&account).map_err(|error| error.to_string())?;
        let doctor = account.doctor().map_err(|error| error.to_string())?;
        Ok::<_, String>((volumes, doctor.problems()))
    })();
    let (volumes, doctor_findings, storage_error) = match storage {
        Ok((volumes, findings)) => (volumes, Some(findings), None),
        Err(error) => (Vec::new(), None, Some(error)),
    };
    LocalMachine {
        name,
        model,
        uptime_seconds,
        volumes,
        doctor_findings,
        storage_error,
    }
}

#[tauri::command]
pub async fn local_machine_info() -> Result<LocalMachine, String> {
    super::blocking(|| Ok(local_machine())).await
}

#[tauri::command]
pub async fn open_moonlight() -> Result<(), String> {
    super::blocking(|| {
        let status = std::process::Command::new("open")
            .args(["-a", "Moonlight"])
            .status()
            .map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err("Could not open Moonlight".into())
        }
    })
    .await
}

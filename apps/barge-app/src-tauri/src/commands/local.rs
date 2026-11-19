//! Read-only information about the machine running Barge.

use serde::Serialize;

#[derive(Serialize)]
pub struct LocalMachine {
    pub name: String,
    pub state: &'static str,
    pub observed_at_unix_ms: u64,
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
    let account = barge::Barge::open_read_only().map_err(|error| error.to_string());
    local_from_account(
        name,
        model,
        uptime_seconds,
        account.as_ref().map_err(Clone::clone),
    )
}

fn local_from_account(
    name: String,
    model: Option<String>,
    uptime_seconds: Option<u64>,
    account: Result<&barge::Barge, String>,
) -> LocalMachine {
    let state = if uptime_seconds.is_some() {
        "awake"
    } else {
        "unknown"
    };
    let observed_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_millis().try_into().unwrap_or(u64::MAX));
    let storage = (|| {
        let account = account?;
        let volumes = crate::storage::volumes(account).map_err(|error| error.to_string())?;
        let doctor = account.doctor().map_err(|error| error.to_string())?;
        Ok::<_, String>((volumes, doctor.problems()))
    })();
    let (volumes, doctor_findings, storage_error) = match storage {
        Ok((volumes, findings)) => (volumes, Some(findings), None),
        Err(error) => (Vec::new(), None, Some(error)),
    };
    LocalMachine {
        name,
        state,
        observed_at_unix_ms,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn local_machine_is_available_without_any_registry() {
        let fixture = barge::testing::TempDir::new("app-local").unwrap();
        let state = fixture.child("missing");
        let account = barge::Barge::open_read_only_with(
            &state,
            Arc::new(barge::FakeOs::new()),
            Arc::new(barge::ManualClock::new(0)),
        )
        .unwrap();
        let local = local_from_account("Test Mac".into(), None, Some(42), Ok(&account));
        assert_eq!(local.name, "Test Mac");
        assert_eq!(local.state, "awake");
        assert!(local.volumes.is_empty());
        assert!(!state.exists());
        assert_eq!(
            local_from_account("Test Mac".into(), None, None, Ok(&account)).state,
            "unknown"
        );
    }
}

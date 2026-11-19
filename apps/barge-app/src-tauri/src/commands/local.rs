//! Read-only information about the machine running Barge.

use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub struct DoctorObservation {
    pub findings: usize,
    pub observed_at_unix_ms: u64,
}
#[derive(Default, Clone)]
pub struct DoctorStatus {
    pub observation: Option<DoctorObservation>,
    pub error: Option<String>,
}
#[derive(Default)]
pub struct DoctorState(pub Arc<Mutex<DoctorStatus>>);

pub fn refresh_doctor(state: &DoctorState) -> Result<(), String> {
    let result = barge::Barge::open_read_only()
        .map_err(|error| error.to_string())
        .and_then(|account| doctor_in(&account));
    state.record(result)
}

fn doctor_in(account: &barge::Barge) -> Result<DoctorObservation, String> {
    let report = account.doctor().map_err(|error| error.to_string())?;
    let observed_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_millis().try_into().unwrap_or(u64::MAX));
    Ok(DoctorObservation {
        findings: report.problems(),
        observed_at_unix_ms,
    })
}

impl DoctorState {
    fn record(&self, result: Result<DoctorObservation, String>) -> Result<(), String> {
        let mut status = self.0.lock().map_err(|error| error.to_string())?;
        match result {
            Ok(observation) => {
                status.observation = Some(observation);
                status.error = None;
                Ok(())
            }
            Err(error) => {
                status.error = Some(error.clone());
                Err(error)
            }
        }
    }
    pub fn refresh_in(&self, account: &barge::Barge) -> Result<(), String> {
        self.record(doctor_in(account))
    }
}

#[derive(Serialize)]
pub struct LocalMachine {
    pub name: String,
    pub state: &'static str,
    pub observed_at_unix_ms: u64,
    pub model: Option<String>,
    pub uptime_seconds: Option<u64>,
    pub volumes: Vec<crate::storage::VolumeSummary>,
    pub doctor_findings: Option<usize>,
    pub doctor_observed_at_unix_ms: Option<u64>,
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
        Ok::<_, String>(volumes)
    })();
    let (volumes, storage_error) = match storage {
        Ok(volumes) => (volumes, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    LocalMachine {
        name,
        state,
        observed_at_unix_ms,
        model,
        uptime_seconds,
        volumes,
        doctor_findings: None,
        doctor_observed_at_unix_ms: None,
        storage_error,
    }
}

#[tauri::command]
pub async fn local_machine_info(
    doctor: tauri::State<'_, DoctorState>,
) -> Result<LocalMachine, String> {
    let cache = doctor.0.clone();
    super::blocking(move || {
        let mut local = local_machine();
        if let Some(observation) = cache.lock().map_err(|error| error.to_string())?.observation {
            local.doctor_findings = Some(observation.findings);
            local.doctor_observed_at_unix_ms = Some(observation.observed_at_unix_ms);
        }
        Ok(local)
    })
    .await
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
    #[test]
    fn local_reads_do_not_run_doctor_but_explicit_refresh_records_it() {
        let fixture = barge::testing::TempDir::new("app-doctor").unwrap();
        let state_dir = fixture.child("missing");
        let account = barge::Barge::open_read_only_with(
            &state_dir,
            Arc::new(barge::FakeOs::new()),
            Arc::new(barge::ManualClock::new(0)),
        )
        .unwrap();
        let doctor = DoctorState::default();
        let local = local_from_account("Mac".into(), None, Some(1), Ok(&account));
        assert!(local.doctor_findings.is_none());
        assert!(doctor.0.lock().unwrap().observation.is_none());
        doctor.refresh_in(&account).unwrap();
        assert_eq!(doctor.0.lock().unwrap().observation.unwrap().findings, 0);
        assert!(!state_dir.exists());
    }
}

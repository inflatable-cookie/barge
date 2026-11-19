//! Doctor's last result, and the Moonlight launch.

use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Serialize)]
pub struct DoctorObservation {
    pub findings: usize,
    pub observed_at_unix_ms: u64,
}
#[derive(Default, Clone, Serialize)]
pub struct DoctorStatus {
    pub observation: Option<DoctorObservation>,
    pub error: Option<String>,
}
#[derive(Default)]
pub struct DoctorState(pub Arc<Mutex<DoctorStatus>>);

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

#[tauri::command]
pub fn doctor_status(doctor: tauri::State<'_, DoctorState>) -> Result<DoctorStatus, String> {
    doctor
        .0
        .lock()
        .map(|status| status.clone())
        .map_err(|error| error.to_string())
}

/// Opens Moonlight. The menu and the window both launch it through here.
pub fn launch_moonlight() -> Result<(), String> {
    let status = std::process::Command::new("open")
        .args(["-a", "Moonlight"])
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Could not open Moonlight".into())
    }
}

#[tauri::command]
pub async fn open_moonlight() -> Result<(), String> {
    super::blocking(launch_moonlight).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_error_preserves_last_observation_and_clears_on_recovery() {
        let state = DoctorState::default();
        state
            .record(Ok(DoctorObservation {
                findings: 0,
                observed_at_unix_ms: 42,
            }))
            .unwrap();
        assert!(state.record(Err("layout unreadable".into())).is_err());
        let status = state.0.lock().unwrap().clone();
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["error"], "layout unreadable");
        assert_eq!(json["observation"]["observed_at_unix_ms"], 42);
        state
            .record(Ok(DoctorObservation {
                findings: 2,
                observed_at_unix_ms: 100,
            }))
            .unwrap();
        let status = state.0.lock().unwrap();
        assert!(status.error.is_none());
        assert_eq!(status.observation.unwrap().findings, 2);
    }
}

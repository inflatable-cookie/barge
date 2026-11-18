//! Machine registry reads and daemon observations.

use std::sync::{Arc, Mutex};

use crate::machines::{client, MachineView, Observations};

#[derive(Default)]
pub struct MachineState(pub Arc<Mutex<Observations>>);

#[tauri::command]
pub async fn machines_list(state: tauri::State<'_, MachineState>) -> Result<Vec<MachineView>, String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || {
        let registry = barge_machine::Registry::discover().map_err(|error| error.to_string())?;
        let machines = registry.list().map_err(|error| error.to_string())?;
        // Network calls happen outside the lock and on a blocking worker.
        let reads: Vec<_> = machines.iter().map(|machine| {
            let result = client(&registry, machine).and_then(|client| client.state())
                .map_err(|error| error.to_string());
            (machine, result)
        }).collect();
        let mut observations = observations.lock().map_err(|error| error.to_string())?;
        observations.retain(&machines);
        Ok(reads.into_iter().map(|(machine, result)| observations.update(machine, result)).collect())
    }).await
}

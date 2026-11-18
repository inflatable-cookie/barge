//! App-owned preferences persisted by Longhorn's config store.
use std::{path::Path, time::Duration};

use longhorn_config::{
    ConfigDomain, ConfigStore, CoordinationAuthority, DomainDescriptor, DomainFilePath,
    DomainIssue, DurabilityRequirement, LoadOutcome, MigrationStep, MutationOptions, StorageClass,
};
use longhorn_core::{
    DomainId, SchemaVersion, SettingsApplyUnitId, SettingsPageId, SettingsRequestId,
    SettingsScopeId,
};
use longhorn_settings::{
    SettingsApplyCommand, SettingsLimits, SettingsLoadCommand, SettingsLoadOutcome,
    SettingsMutationResult, SettingsOpaqueValue, SettingsProtocolVersion,
    SettingsRegistryGeneration,
};
use longhorn_settings_config::ConfigSettingsApplyUnit;
use longhorn_storage_layout::StorageRoots;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub launch_at_login: bool,
    pub refresh_interval: u64,
    pub theme: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            launch_at_login: false,
            refresh_interval: 10,
            theme: "system".into(),
        }
    }
}

pub struct Domain {
    descriptor: DomainDescriptor,
}
impl Domain {
    pub(crate) fn new() -> Result<Self, String> {
        let id = DomainId::new("barge.preferences").map_err(|e| e.to_string())?;
        let version = SchemaVersion::new(1).map_err(|e| e.to_string())?;
        let path = DomainFilePath::new("preferences.json").map_err(|e| e.to_string())?;
        let descriptor = DomainDescriptor::new(id, version, StorageClass::UserConfig, Some(path))
            .map_err(|e| e.to_string())?;
        Ok(Self { descriptor })
    }
}
impl ConfigDomain for Domain {
    type Value = Preferences;
    fn descriptor(&self) -> &DomainDescriptor {
        &self.descriptor
    }
    fn default_value(&self) -> Preferences {
        Preferences::default()
    }
    fn decode(&self, value: Value) -> Result<Preferences, DomainIssue> {
        serde_json::from_value(value).map_err(|e| DomainIssue::new("decode", e.to_string()))
    }
    fn encode(&self, value: &Preferences) -> Result<Value, DomainIssue> {
        serde_json::to_value(value).map_err(|e| DomainIssue::new("encode", e.to_string()))
    }
    fn validate(&self, value: &Preferences) -> Result<(), DomainIssue> {
        if !matches!(value.theme.as_str(), "system" | "light" | "dark")
            || !(1..=3600).contains(&value.refresh_interval)
        {
            return Err(DomainIssue::new(
                "preferences",
                "Invalid theme or refresh interval",
            ));
        }
        Ok(())
    }
    fn validate_raw(&self, version: SchemaVersion, value: &Value) -> Result<(), DomainIssue> {
        if version != self.descriptor.schema_version() {
            return Err(DomainIssue::new(
                "version",
                "Unsupported preferences version",
            ));
        }
        self.validate(&self.decode(value.clone())?)
    }
    fn migrate_one(
        &self,
        _: SchemaVersion,
        _: Value,
    ) -> Result<Option<MigrationStep>, DomainIssue> {
        Ok(None)
    }
}

pub struct Store {
    store: ConfigStore,
    unit: ConfigSettingsApplyUnit<Domain, crate::settings::Adapter>,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self, String> {
        let paths = [
            "config", "data", "state", "cache", "runtime", "logs", "backups",
        ]
        .map(|name| root.join(name));
        for path in &paths {
            std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
        }
        let roots = StorageRoots::new(
            &paths[0], &paths[1], &paths[2], &paths[3], &paths[4], &paths[5], &paths[6],
        )
        .map_err(|e| e.to_string())?;
        let coordination = CoordinationAuthority::new(&paths[1]).map_err(|e| e.to_string())?;
        let mut store = ConfigStore::new(roots, coordination);
        let domain = Domain::new()?;
        store.register(&domain).map_err(|e| e.to_string())?;
        let registry = crate::settings::registry()?;
        let unit_id = SettingsApplyUnitId::new("barge:apply").map_err(|e| e.to_string())?;
        let unit =
            ConfigSettingsApplyUnit::new(&registry, &unit_id, domain, crate::settings::Adapter)
                .map_err(|e| e.to_string())?;
        Ok(Self { store, unit })
    }
    pub fn get(&self) -> Result<Preferences, String> {
        match self
            .store
            .load(self.unit.domain())
            .map_err(|e| e.to_string())?
        {
            LoadOutcome::Ready(loaded) => Ok(loaded.value),
            other => Err(format!("Preferences unavailable: {other:?}")),
        }
    }
    pub fn set(&self, value: Preferences) -> Result<(), String> {
        let scope_id = SettingsScopeId::new("barge:preferences").map_err(|e| e.to_string())?;
        let load = SettingsLoadCommand {
            protocol_version: SettingsProtocolVersion::CURRENT,
            request_id: SettingsRequestId::new("barge:load").map_err(|e| e.to_string())?,
            registry_generation: SettingsRegistryGeneration::new(1),
            scope_id: scope_id.clone(),
            known_authority: None,
        };
        let snapshot = match self
            .unit
            .load(&self.store, &load, Duration::from_secs(1))
            .map_err(|e| e.to_string())?
        {
            SettingsLoadOutcome::Loaded { snapshot } if snapshot.recovery.is_none() => snapshot,
            other => return Err(format!("Settings unavailable: {other:?}")),
        };
        let intent = SettingsOpaqueValue::new(
            1,
            serde_json::to_value(value).map_err(|e| e.to_string())?,
            SettingsLimits::default(),
        )
        .map_err(|e| e.to_string())?;
        let command = SettingsApplyCommand {
            protocol_version: SettingsProtocolVersion::CURRENT,
            request_id: SettingsRequestId::new("barge:apply-request").map_err(|e| e.to_string())?,
            page_id: SettingsPageId::new("barge:settings").map_err(|e| e.to_string())?,
            apply_unit_id: SettingsApplyUnitId::new("barge:apply").map_err(|e| e.to_string())?,
            scope_id,
            authority: snapshot.authority,
            intent,
        };
        match self
            .unit
            .apply(
                &self.store,
                &command,
                MutationOptions::new(Duration::from_secs(1), DurabilityRequirement::Atomic),
            )
            .map_err(|e| e.to_string())?
        {
            SettingsMutationResult::Applied { .. } => Ok(()),
            other => Err(format!("Settings were not saved: {other:?}")),
        }
    }
}

impl longhorn_tauri_settings::SettingsAuthority for Store {
    fn registry(
        &mut self,
        caller: &str,
    ) -> Result<
        longhorn_settings::SettingsRegistrySnapshot,
        longhorn_tauri_settings::SettingsHostError,
    > {
        authorize(caller)?;
        crate::settings::registry()
            .map(|registry| (&registry).into())
            .map_err(|e| longhorn_tauri_settings::SettingsHostError::authority(e, false))
    }
    fn load(
        &mut self,
        caller: &str,
        command: SettingsLoadCommand,
    ) -> Result<SettingsLoadOutcome, longhorn_tauri_settings::SettingsHostError> {
        authorize(caller)?;
        self.unit
            .load(&self.store, &command, Duration::from_secs(1))
            .map_err(|e| longhorn_tauri_settings::SettingsHostError::authority(e.to_string(), true))
    }
    fn apply(
        &mut self,
        _: &str,
        _: SettingsApplyCommand,
    ) -> Result<SettingsMutationResult, longhorn_tauri_settings::SettingsHostError> {
        Err(longhorn_tauri_settings::SettingsHostError::authority(
            "Use Barge's settings command for system activation",
            false,
        ))
    }
    fn reset(
        &mut self,
        _: &str,
        _: longhorn_settings::SettingsResetCommand,
    ) -> Result<SettingsMutationResult, longhorn_tauri_settings::SettingsHostError> {
        Err(longhorn_tauri_settings::SettingsHostError::authority(
            "Use Barge's settings command for system activation",
            false,
        ))
    }
}
fn authorize(caller: &str) -> Result<(), longhorn_tauri_settings::SettingsHostError> {
    if caller == "main" || caller == "popover" {
        Ok(())
    } else {
        Err(longhorn_tauri_settings::SettingsHostError::authority(
            "Window is not authorized",
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barge::testing::TempDir;

    #[test]
    fn preferences_survive_reopening_and_reject_invalid_values() {
        let fixture = TempDir::new("app-preferences").unwrap();
        let store = Store::open(fixture.path()).unwrap();
        assert_eq!(store.get().unwrap().refresh_interval, 10);
        store
            .set(Preferences {
                launch_at_login: false,
                refresh_interval: 30,
                theme: "dark".into(),
            })
            .unwrap();
        let reopened = Store::open(fixture.path()).unwrap();
        let loaded = reopened.get().unwrap();
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.refresh_interval, 30);
        assert!(reopened
            .set(Preferences {
                theme: "invalid".into(),
                ..loaded
            })
            .is_err());
        assert_eq!(reopened.get().unwrap().theme, "dark");
    }
}

//! App-owned preferences persisted by Longhorn's config store.
use std::{path::Path, time::Duration};

use longhorn_config::{ConfigDomain, ConfigStore, CoordinationAuthority, DomainDescriptor, DomainFilePath, DomainIssue, DurabilityRequirement, LoadOutcome, MigrationStep, MutationOptions, StorageClass};
use longhorn_core::{DomainId, SchemaVersion};
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
        Self { launch_at_login: false, refresh_interval: 10, theme: "system".into() }
    }
}

pub struct Domain { descriptor: DomainDescriptor }
impl Domain {
    fn new() -> Result<Self, String> {
        let id = DomainId::new("barge.preferences").map_err(|e| e.to_string())?;
        let version = SchemaVersion::new(1).map_err(|e| e.to_string())?;
        let path = DomainFilePath::new("preferences.json").map_err(|e| e.to_string())?;
        let descriptor = DomainDescriptor::new(id, version, StorageClass::UserConfig, Some(path)).map_err(|e| e.to_string())?;
        Ok(Self { descriptor })
    }
}
impl ConfigDomain for Domain {
    type Value = Preferences;
    fn descriptor(&self) -> &DomainDescriptor { &self.descriptor }
    fn default_value(&self) -> Preferences { Preferences::default() }
    fn decode(&self, value: Value) -> Result<Preferences, DomainIssue> { serde_json::from_value(value).map_err(|e| DomainIssue::new("decode", e.to_string())) }
    fn encode(&self, value: &Preferences) -> Result<Value, DomainIssue> { serde_json::to_value(value).map_err(|e| DomainIssue::new("encode", e.to_string())) }
    fn validate(&self, value: &Preferences) -> Result<(), DomainIssue> {
        if !matches!(value.theme.as_str(), "system" | "light" | "dark") || !(1..=3600).contains(&value.refresh_interval) {
            return Err(DomainIssue::new("preferences", "Invalid theme or refresh interval"));
        }
        Ok(())
    }
    fn validate_raw(&self, version: SchemaVersion, value: &Value) -> Result<(), DomainIssue> {
        if version != self.descriptor.schema_version() { return Err(DomainIssue::new("version", "Unsupported preferences version")); }
        self.validate(&self.decode(value.clone())?)
    }
    fn migrate_one(&self, _: SchemaVersion, _: Value) -> Result<Option<MigrationStep>, DomainIssue> { Ok(None) }
}

pub struct Store { store: ConfigStore, domain: Domain }
impl Store {
    pub fn open(root: &Path) -> Result<Self, String> {
        let paths = ["config", "data", "state", "cache", "runtime", "logs", "backups"].map(|name| root.join(name));
        for path in &paths { std::fs::create_dir_all(path).map_err(|e| e.to_string())?; }
        let roots = StorageRoots::new(&paths[0], &paths[1], &paths[2], &paths[3], &paths[4], &paths[5], &paths[6]).map_err(|e| e.to_string())?;
        let coordination = CoordinationAuthority::new(&paths[1]).map_err(|e| e.to_string())?;
        let mut store = ConfigStore::new(roots, coordination);
        let domain = Domain::new()?;
        store.register(&domain).map_err(|e| e.to_string())?;
        Ok(Self { store, domain })
    }
    pub fn get(&self) -> Result<Preferences, String> {
        match self.store.load(&self.domain).map_err(|e| e.to_string())? {
            LoadOutcome::Ready(loaded) => Ok(loaded.value),
            other => Err(format!("Preferences unavailable: {other:?}")),
        }
    }
    pub fn set(&self, value: Preferences) -> Result<(), String> {
        self.domain.validate(&value).map_err(|e| format!("{e:?}"))?;
        self.store.mutate(&self.domain, MutationOptions::new(Duration::from_secs(1), DurabilityRequirement::Atomic), |current| { *current = value; Ok(()) }).map_err(|e| e.to_string())?;
        Ok(())
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
        store.set(Preferences { launch_at_login: false, refresh_interval: 30, theme: "dark".into() }).unwrap();
        let reopened = Store::open(fixture.path()).unwrap();
        let loaded = reopened.get().unwrap();
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.refresh_interval, 30);
        assert!(reopened.set(Preferences { theme: "invalid".into(), ..loaded }).is_err());
        assert_eq!(reopened.get().unwrap().theme, "dark");
    }
}

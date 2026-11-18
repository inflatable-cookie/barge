//! App-owned preferences persisted by Longhorn's config store.
//!
//! One [`Store`] is the app's only preferences authority: the renderer's
//! commands and Longhorn's settings registry share the same handle, so neither
//! can read a value the other has not written.
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

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
    /// The window opens on launch, so the app is reachable even when macOS
    /// parks its menu-bar item behind the notch. Defaults on for that reason;
    /// files written before the setting existed get the same answer.
    #[serde(default = "show_window_at_launch")]
    pub show_window_at_launch: bool,
}

fn show_window_at_launch() -> bool {
    true
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            launch_at_login: false,
            refresh_interval: 10,
            theme: "system".into(),
            show_window_at_launch: true,
        }
    }
}

/// The operating system's launch-at-login registration.
///
/// Behind a seam so the rollback path is testable: the real implementation asks
/// the OS, a test one answers from memory.
pub trait Autostart {
    fn enable(&self) -> Result<(), String>;
    fn disable(&self) -> Result<(), String>;
    fn is_enabled(&self) -> Result<bool, String>;
}

/// The persisted half of the preferences authority.
pub trait PersistedPreferences {
    fn get(&self) -> Result<Preferences, String>;
    fn set(&self, value: Preferences) -> Result<(), String>;
}

/// Saves `requested` and reports the preferences that are actually stored.
///
/// Launch at login goes to the OS first, and the OS's own answer is what gets
/// persisted, so the app never claims a login item the operator does not have.
/// A failed save puts the OS back where it was. The caller holds the store lock
/// across the whole read-modify-write, so two saves cannot interleave.
pub fn persist(
    store: &dyn PersistedPreferences,
    autostart: &dyn Autostart,
    requested: Preferences,
) -> Result<Preferences, String> {
    let previous = store.get()?;
    let changed = previous.launch_at_login != requested.launch_at_login;
    let launch_at_login = if changed {
        if let Err(error) = apply_autostart(autostart, requested.launch_at_login) {
            return Err(format!("Could not change launch at login: {error}"));
        }
        match autostart.is_enabled() {
            Ok(observed) => observed,
            Err(error) => {
                return Err(restored(
                    autostart,
                    previous.launch_at_login,
                    format!("Could not read launch at login back from the system: {error}"),
                ));
            }
        }
    } else {
        previous.launch_at_login
    };
    let saved = Preferences {
        launch_at_login,
        ..requested
    };
    match store.set(saved.clone()) {
        Ok(()) => Ok(saved),
        Err(error) => {
            let message = format!("Could not save settings: {error}");
            if changed {
                Err(restored(autostart, previous.launch_at_login, message))
            } else {
                Err(message)
            }
        }
    }
}

fn apply_autostart(autostart: &dyn Autostart, enabled: bool) -> Result<(), String> {
    if enabled {
        autostart.enable()
    } else {
        autostart.disable()
    }
}

/// Puts the OS registration back, and says so if that also fails.
fn restored(autostart: &dyn Autostart, launch_at_login: bool, message: String) -> String {
    match apply_autostart(autostart, launch_at_login) {
        Ok(()) => message,
        Err(error) => format!("{message}; launch at login is still changed: {error}"),
    }
}

/// The real OS registration, through Tauri's autostart plugin.
pub struct LoginItem(pub tauri::AppHandle);

impl Autostart for LoginItem {
    fn enable(&self) -> Result<(), String> {
        use tauri_plugin_autostart::ManagerExt;
        self.0
            .autolaunch()
            .enable()
            .map_err(|error| error.to_string())
    }

    fn disable(&self) -> Result<(), String> {
        use tauri_plugin_autostart::ManagerExt;
        self.0
            .autolaunch()
            .disable()
            .map_err(|error| error.to_string())
    }

    fn is_enabled(&self) -> Result<bool, String> {
        use tauri_plugin_autostart::ManagerExt;
        self.0
            .autolaunch()
            .is_enabled()
            .map_err(|error| error.to_string())
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

struct Inner {
    store: ConfigStore,
    unit: ConfigSettingsApplyUnit<Domain, crate::settings::Adapter>,
}

impl PersistedPreferences for Inner {
    fn get(&self) -> Result<Preferences, String> {
        match self
            .store
            .load(self.unit.domain())
            .map_err(|e| e.to_string())?
        {
            LoadOutcome::Ready(loaded) => Ok(loaded.value),
            other => Err(format!("Preferences unavailable: {other:?}")),
        }
    }

    fn set(&self, value: Preferences) -> Result<(), String> {
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

/// The single preferences handle shared by the commands and the settings
/// registry: one store, one lock.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
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
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { store, unit })),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, String> {
        self.inner
            .lock()
            .map_err(|_| "Preferences are unavailable: the store lock was poisoned".to_string())
    }

    pub fn get(&self) -> Result<Preferences, String> {
        self.lock()?.get()
    }

    /// Writes preferences without touching the OS registration. Tests seed a
    /// store with it; the app's path is [`Store::replace`].
    #[cfg(test)]
    pub fn set(&self, value: Preferences) -> Result<(), String> {
        self.lock()?.set(value)
    }

    /// Applies `requested` under the store lock and returns what was stored.
    pub fn replace(
        &self,
        requested: Preferences,
        autostart: &dyn Autostart,
    ) -> Result<Preferences, String> {
        persist(&*self.lock()?, autostart, requested)
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
        let inner = self
            .lock()
            .map_err(|error| longhorn_tauri_settings::SettingsHostError::authority(error, false))?;
        inner
            .unit
            .load(&inner.store, &command, Duration::from_secs(1))
            .map_err(|e| longhorn_tauri_settings::SettingsHostError::authority(e.to_string(), true))
    }
    fn apply(
        &mut self,
        caller: &str,
        command: SettingsApplyCommand,
    ) -> Result<SettingsMutationResult, longhorn_tauri_settings::SettingsHostError> {
        authorize(caller)?;
        let inner = self
            .lock()
            .map_err(|error| longhorn_tauri_settings::SettingsHostError::authority(error, false))?;
        // The page advertises this apply unit, so this operation is supported:
        // it commits through the same unit and store Barge's command uses. The
        // unit's adapter refuses launch-at-login intent, because that one needs
        // the OS registration and its rollback.
        inner
            .unit
            .apply(
                &inner.store,
                &command,
                MutationOptions::new(Duration::from_secs(1), DurabilityRequirement::Atomic),
            )
            .map_err(|e| longhorn_tauri_settings::SettingsHostError::authority(e.to_string(), true))
    }
    fn reset(
        &mut self,
        _: &str,
        _: longhorn_settings::SettingsResetCommand,
    ) -> Result<SettingsMutationResult, longhorn_tauri_settings::SettingsHostError> {
        // No page advertises reset, so this cannot be reached through the
        // registry; refuse it rather than pretending otherwise.
        Err(longhorn_tauri_settings::SettingsHostError::authority(
            "Barge's settings page does not support reset",
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
    use std::sync::Mutex;

    use barge::testing::TempDir;
    use longhorn_tauri_settings::SettingsAuthority;

    use super::*;

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
                ..Preferences::default()
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

    #[test]
    fn preferences_written_before_the_window_setting_default_to_showing_it() {
        let legacy = serde_json::json!({
            "launch_at_login": false,
            "refresh_interval": 30,
            "theme": "light",
        });
        let decoded = Domain::new()
            .unwrap()
            .decode(legacy)
            .expect("older files still decode");
        assert!(decoded.show_window_at_launch);
        assert_eq!(decoded.refresh_interval, 30);
    }

    #[test]
    fn clones_share_one_store() {
        let fixture = TempDir::new("app-preferences-shared").unwrap();
        let store = Store::open(fixture.path()).unwrap();
        store
            .clone()
            .set(Preferences {
                theme: "light".into(),
                ..Preferences::default()
            })
            .unwrap();
        assert_eq!(store.get().unwrap().theme, "light");
    }

    /// Saves that fail still hold the stored value.
    struct Failing {
        value: Mutex<Preferences>,
    }

    impl Failing {
        fn new(value: Preferences) -> Self {
            Self {
                value: Mutex::new(value),
            }
        }
    }

    impl PersistedPreferences for Failing {
        fn get(&self) -> Result<Preferences, String> {
            Ok(self.value.lock().unwrap().clone())
        }

        fn set(&self, _: Preferences) -> Result<(), String> {
            Err("the config store is unavailable".into())
        }
    }

    #[derive(Default)]
    struct Login(Mutex<Vec<String>>);

    impl Login {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        fn record(&self, call: &str) {
            self.0.lock().unwrap().push(call.to_owned());
        }
    }

    impl Autostart for Login {
        fn enable(&self) -> Result<(), String> {
            self.record("enable");
            Ok(())
        }
        fn disable(&self) -> Result<(), String> {
            self.record("disable");
            Ok(())
        }
        fn is_enabled(&self) -> Result<bool, String> {
            self.record("is_enabled");
            Ok(true)
        }
    }

    #[test]
    fn a_failed_save_puts_launch_at_login_back() {
        let store = Failing::new(Preferences::default());
        let login = Login::default();
        let error = persist(
            &store,
            &login,
            Preferences {
                launch_at_login: true,
                ..Preferences::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("Could not save settings"), "{error}");
        assert_eq!(login.calls(), ["enable", "is_enabled", "disable"]);
        assert!(!store.get().unwrap().launch_at_login);
    }

    #[test]
    fn the_stored_value_is_the_os_answer() {
        /// The system always reports disabled, whatever was asked for.
        struct Ignores(Login);

        impl Autostart for Ignores {
            fn enable(&self) -> Result<(), String> {
                self.0.record("enable");
                Ok(())
            }
            fn disable(&self) -> Result<(), String> {
                self.0.record("disable");
                Ok(())
            }
            fn is_enabled(&self) -> Result<bool, String> {
                self.0.record("is_enabled");
                Ok(false)
            }
        }

        struct Memory(Mutex<Preferences>);

        impl PersistedPreferences for Memory {
            fn get(&self) -> Result<Preferences, String> {
                Ok(self.0.lock().unwrap().clone())
            }
            fn set(&self, value: Preferences) -> Result<(), String> {
                *self.0.lock().unwrap() = value;
                Ok(())
            }
        }

        let store = Memory(Mutex::new(Preferences::default()));
        let login = Ignores(Login::default());
        let saved = persist(
            &store,
            &login,
            Preferences {
                launch_at_login: true,
                refresh_interval: 30,
                theme: "dark".into(),
                ..Preferences::default()
            },
        )
        .unwrap();
        assert!(!saved.launch_at_login);
        assert_eq!(saved.refresh_interval, 30);
        assert!(!store.get().unwrap().launch_at_login);
        assert_eq!(store.get().unwrap().theme, "dark");
        assert_eq!(login.0.calls(), ["enable", "is_enabled"]);
    }
    fn load_command() -> SettingsLoadCommand {
        SettingsLoadCommand {
            protocol_version: SettingsProtocolVersion::CURRENT,
            request_id: SettingsRequestId::new("barge:test-load").unwrap(),
            registry_generation: SettingsRegistryGeneration::new(1),
            scope_id: SettingsScopeId::new("barge:preferences").unwrap(),
            known_authority: None,
        }
    }

    fn apply_command(
        snapshot: longhorn_settings::SettingsScopeSnapshot,
        intent: Preferences,
    ) -> SettingsApplyCommand {
        SettingsApplyCommand {
            protocol_version: SettingsProtocolVersion::CURRENT,
            request_id: SettingsRequestId::new("barge:test-apply").unwrap(),
            page_id: SettingsPageId::new("barge:settings").unwrap(),
            apply_unit_id: SettingsApplyUnitId::new("barge:apply").unwrap(),
            scope_id: SettingsScopeId::new("barge:preferences").unwrap(),
            authority: snapshot.authority,
            intent: SettingsOpaqueValue::new(
                1,
                serde_json::to_value(intent).unwrap(),
                SettingsLimits::default(),
            )
            .unwrap(),
        }
    }

    fn loaded(store: &mut Store) -> longhorn_settings::SettingsScopeSnapshot {
        match store.load("main", load_command()).unwrap() {
            SettingsLoadOutcome::Loaded { snapshot } => snapshot,
            other => panic!("expected a loaded scope, got {other:?}"),
        }
    }

    #[test]
    fn the_settings_registry_writes_through_the_same_store() {
        let fixture = TempDir::new("app-preferences-registry").unwrap();
        let mut store = Store::open(fixture.path()).unwrap();
        let snapshot = loaded(&mut store);
        let applied = store
            .apply(
                "main",
                apply_command(
                    snapshot,
                    Preferences {
                        theme: "dark".into(),
                        refresh_interval: 30,
                        ..Preferences::default()
                    },
                ),
            )
            .unwrap();
        assert!(
            matches!(applied, SettingsMutationResult::Applied { .. }),
            "{applied:?}"
        );
        assert_eq!(store.get().unwrap().theme, "dark");
        assert_eq!(store.get().unwrap().refresh_interval, 30);
    }

    #[test]
    fn the_settings_registry_leaves_launch_at_login_to_the_command() {
        let fixture = TempDir::new("app-preferences-registry-login").unwrap();
        let mut store = Store::open(fixture.path()).unwrap();
        let snapshot = loaded(&mut store);
        let refused = store
            .apply(
                "main",
                apply_command(
                    snapshot,
                    Preferences {
                        launch_at_login: true,
                        ..Preferences::default()
                    },
                ),
            )
            .unwrap();
        match refused {
            SettingsMutationResult::Rejected { rejection, .. } => assert_eq!(
                rejection.code,
                longhorn_settings::SettingsRejectionCode::Unsupported
            ),
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(!store.get().unwrap().launch_at_login);
    }
}

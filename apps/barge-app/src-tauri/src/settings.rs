//! Longhorn settings registry and the preferences apply unit.
use crate::preferences::{Domain, Preferences};
use longhorn_config::ConfigDomain;
use longhorn_core::{
    SettingsApplyUnitId, SettingsEntryId, SettingsModuleId, SettingsPageId, SettingsRendererId,
    SettingsScopeId, SettingsSectionId,
};
use longhorn_settings::{
    SettingsActivationRequirement, SettingsApplyUnitDefinition, SettingsEditability,
    SettingsEffectiveSource, SettingsLimits, SettingsModuleDefinition, SettingsMutationOutcome,
    SettingsMutationTiming, SettingsOpaqueValue, SettingsPageDefinition, SettingsPageFeatures,
    SettingsRegistry, SettingsRegistryBuilder, SettingsRegistryGeneration, SettingsRejection,
    SettingsRejectionCode, SettingsRendererDefinition, SettingsScopeDefinition,
    SettingsSectionDefinition, SettingsValueProjection,
};
use longhorn_settings_config::{
    SettingsCommittedMutation, SettingsConfigAdapter, SettingsConfigProjection,
    SettingsConfigProjectionError,
};

pub fn registry() -> Result<SettingsRegistry, String> {
    let module = SettingsModuleId::new("barge:module").map_err(|e| e.to_string())?;
    let section = SettingsSectionId::new("barge:section").map_err(|e| e.to_string())?;
    let renderer = SettingsRendererId::new("barge:renderer").map_err(|e| e.to_string())?;
    let scope = SettingsScopeId::new("barge:preferences").map_err(|e| e.to_string())?;
    let unit = SettingsApplyUnitId::new("barge:apply").map_err(|e| e.to_string())?;
    let page = SettingsPageId::new("barge:settings").map_err(|e| e.to_string())?;
    let mut b = SettingsRegistryBuilder::new(
        SettingsRegistryGeneration::new(1),
        SettingsLimits::default(),
    );
    b.register_module(SettingsModuleDefinition {
        id: module.clone(),
        label: "Preferences".into(),
        order: 0,
    })
    .map_err(|e| e.to_string())?;
    b.register_section(SettingsSectionDefinition {
        id: section.clone(),
        module_id: module.clone(),
        label: "Preferences".into(),
        order: 0,
    })
    .map_err(|e| e.to_string())?;
    b.register_renderer(SettingsRendererDefinition {
        id: renderer.clone(),
        module_id: module.clone(),
    })
    .map_err(|e| e.to_string())?;
    b.register_scope(SettingsScopeDefinition {
        id: scope.clone(),
        module_id: module.clone(),
    })
    .map_err(|e| e.to_string())?;
    b.register_apply_unit(SettingsApplyUnitDefinition {
        id: unit.clone(),
        module_id: module.clone(),
        scope_id: scope.clone(),
        timing: SettingsMutationTiming::Immediate,
        // `settings::Adapter` writes Barge's own preferences; activating
        // launch at login stays Barge's command, so the registry offers no
        // reset through this unit.
        reset_supported: false,
    })
    .map_err(|e| e.to_string())?;
    b.register_page(SettingsPageDefinition {
        id: page,
        module_id: module,
        section_id: section,
        renderer_id: renderer,
        label: "Preferences".into(),
        keywords: vec![],
        order: 0,
        anchors: vec![],
        required_capabilities: vec![],
        readable_scope_ids: vec![scope],
        writable_apply_unit_ids: vec![unit],
        // Barge's authority writes through `SettingsAuthority::apply` and
        // refuses reset, so no page advertises reset.
        features: SettingsPageFeatures::default(),
    })
    .map_err(|e| e.to_string())?;
    b.seal([]).map_err(|e| e.to_string())
}

pub struct Adapter;
fn rejection() -> SettingsRejection {
    SettingsRejection {
        code: SettingsRejectionCode::InvalidIntent,
        diagnostic: None,
    }
}
fn entry() -> Result<SettingsEntryId, SettingsRejection> {
    SettingsEntryId::new("barge:preferences").map_err(|_| rejection())
}
fn opaque(value: serde_json::Value) -> Result<SettingsOpaqueValue, SettingsConfigProjectionError> {
    SettingsOpaqueValue::new(1, value, SettingsLimits::default())
        .map_err(|e| SettingsConfigProjectionError::new("preferences", e.to_string()))
}
impl SettingsConfigAdapter<Preferences> for Adapter {
    type Intent = Preferences;
    fn project(
        &self,
        value: &Preferences,
    ) -> Result<SettingsConfigProjection, SettingsConfigProjectionError> {
        let encoded = serde_json::to_value(value)
            .map_err(|e| SettingsConfigProjectionError::new("preferences", e.to_string()))?;
        let default = serde_json::to_value(Preferences::default())
            .map_err(|e| SettingsConfigProjectionError::new("preferences", e.to_string()))?;
        let actual = opaque(encoded)?;
        let id = entry()
            .map_err(|_| SettingsConfigProjectionError::new("preferences", "Invalid entry ID"))?;
        SettingsConfigProjection::new(
            vec![SettingsValueProjection {
                entry_id: id,
                configured: Some(actual.clone()),
                effective: actual,
                compiled_default: opaque(default)?,
                effective_source: SettingsEffectiveSource::UserConfiguration,
                policy: None,
                editability: SettingsEditability::Editable,
                source_diagnostics: vec![],
            }],
            vec![],
        )
    }
    fn decode_intent(
        &self,
        intent: &SettingsOpaqueValue,
    ) -> Result<Preferences, SettingsRejection> {
        if intent.codec_version() != 1 {
            return Err(rejection());
        }
        serde_json::from_value(intent.value().clone()).map_err(|_| rejection())
    }
    fn targeted_entries(&self, _: &Preferences) -> Vec<SettingsEntryId> {
        entry().into_iter().collect()
    }
    fn validate_intent(
        &self,
        current: &Preferences,
        intent: &Preferences,
        _: &SettingsConfigProjection,
    ) -> Result<(), SettingsRejection> {
        // Launch at login is a system login item, and changing it belongs to
        // Barge's settings command, which drives the OS and rolls back. The
        // registry writes the rest of the scope.
        if intent.launch_at_login != current.launch_at_login {
            return Err(SettingsRejection {
                code: SettingsRejectionCode::Unsupported,
                diagnostic: SettingsOpaqueValue::new(
                    1,
                    serde_json::json!("Launch at login changes through Barge's settings command"),
                    SettingsLimits::default(),
                )
                .ok(),
            });
        }
        Domain::new()
            .map_err(|_| rejection())?
            .validate(intent)
            .map_err(|_| rejection())
    }
    fn patch(
        &self,
        current: &mut Preferences,
        intent: &Preferences,
    ) -> Result<(), SettingsRejection> {
        *current = intent.clone();
        Ok(())
    }
    fn reset(
        &self,
        current: &mut Preferences,
        ids: &[SettingsEntryId],
    ) -> Result<(), SettingsRejection> {
        if ids.iter().any(|id| Some(id) != entry().ok().as_ref()) {
            return Err(rejection());
        }
        *current = Preferences::default();
        Ok(())
    }
    fn activation_after_commit(
        &self,
        _: SettingsCommittedMutation<'_, Preferences>,
        _: SettingsMutationTiming,
        _: SettingsMutationOutcome,
        _: &SettingsConfigProjection,
    ) -> Vec<SettingsActivationRequirement> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_advertises_only_what_the_authority_supports() {
        let registry = registry().unwrap();
        let page = registry.pages().next().expect("one page");
        // Writing the scope is supported through the apply unit; reset is not.
        assert_eq!(page.writable_apply_unit_ids.len(), 1);
        assert!(!page.features.reset);
        let unit = registry
            .apply_unit(&page.writable_apply_unit_ids[0])
            .expect("the page's apply unit is registered");
        assert!(!unit.reset_supported);
        assert_eq!(page.readable_scope_ids, vec![unit.scope_id.clone()]);
    }
}

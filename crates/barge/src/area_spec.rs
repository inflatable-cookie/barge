//! Area declarations and their stored form.

use crate::error::{Error, Result};
use crate::state::{validate_name, validate_relative_path};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The schema name of a stored area declaration.
pub const AREA_SCHEMA: &str = "barge.area/1";

/// What an area does when its volume is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Absent {
    /// Report the volume to attach; caches and scratch use this.
    Fail,
    /// Use an explicit internal path and say so in the result and in status.
    Fallback { path: PathBuf },
}

/// The `<namespace>/<area>` name of an area.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AreaKey {
    namespace: String,
    name: String,
}

impl AreaKey {
    /// Validate and build a key.
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Result<Self> {
        let key = Self {
            namespace: namespace.into(),
            name: name.into(),
        };
        validate_name("area namespace", &key.namespace)?;
        validate_name("area name", &key.name)?;
        Ok(key)
    }

    /// Parse `<namespace>/<area>`.
    pub fn parse(text: &str) -> Result<Self> {
        let (namespace, name) = text.split_once('/').ok_or_else(|| {
            Error::usage(format!("area {text:?} must be written <namespace>/<area>"))
        })?;
        Self::new(namespace, name)
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `<namespace>/<area>`, the name a consumer and a marker use.
    pub fn label(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    /// The state file's stem, `<namespace>.<area>`.
    pub(crate) fn file_stem(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
}

/// A consumer's area declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaSpec {
    pub namespace: String,
    pub name: String,
    pub volume: String,
    /// Folder inside the volume base; `None` means `<namespace>/<area>`.
    pub folder: Option<String>,
    pub absent: Absent,
    pub soft_cap_bytes: Option<u64>,
    pub counts_toward_reserve: bool,
}

impl AreaSpec {
    /// A declaration whose path defaults to `<root_dir>/<namespace>/<area>`, that
    /// fails when its volume is absent and counts toward the reserve.
    pub fn new(
        namespace: impl Into<String>,
        name: impl Into<String>,
        volume: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            volume: volume.into(),
            folder: None,
            absent: Absent::Fail,
            soft_cap_bytes: None,
            counts_toward_reserve: true,
        }
    }

    pub fn folder(mut self, folder: impl Into<String>) -> Self {
        self.folder = Some(folder.into());
        self
    }

    pub fn absent(mut self, absent: Absent) -> Self {
        self.absent = absent;
        self
    }

    pub fn soft_cap_bytes(mut self, soft_cap_bytes: Option<u64>) -> Self {
        self.soft_cap_bytes = soft_cap_bytes;
        self
    }

    pub fn counts_toward_reserve(mut self, counts: bool) -> Self {
        self.counts_toward_reserve = counts;
        self
    }

    pub(crate) fn into_record(self) -> AreaRecord {
        AreaRecord {
            schema: AREA_SCHEMA.to_string(),
            namespace: self.namespace,
            name: self.name,
            volume: self.volume,
            folder: self.folder,
            absent: self.absent,
            soft_cap_bytes: self.soft_cap_bytes,
            counts_toward_reserve: self.counts_toward_reserve,
        }
    }
}

/// A stored area declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaRecord {
    pub schema: String,
    pub namespace: String,
    pub name: String,
    pub volume: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub absent: Absent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_cap_bytes: Option<u64>,
    pub counts_toward_reserve: bool,
}

impl AreaRecord {
    /// The area's key.
    pub fn key(&self) -> AreaKey {
        AreaKey {
            namespace: self.namespace.clone(),
            name: self.name.clone(),
        }
    }

    /// The folder this area uses inside the volume base.
    pub fn effective_folder(&self) -> String {
        self.folder
            .clone()
            .unwrap_or_else(|| format!("{}/{}", self.namespace, self.name))
    }

    /// Validate every stored field before the record is used as truth.
    pub fn validate(&self) -> Result<()> {
        if self.schema != AREA_SCHEMA {
            return Err(Error::unknown_state(format!(
                "area {}/{} has schema {}, not {AREA_SCHEMA}",
                self.namespace, self.name, self.schema
            )));
        }
        validate_name("area namespace", &self.namespace)?;
        validate_name("area name", &self.name)?;
        crate::state::validate_volume_name("area volume", &self.volume)?;
        if let Some(folder) = &self.folder {
            if folder.is_empty() {
                return Err(Error::usage(format!(
                    "area {}/{} folder override must not be empty",
                    self.namespace, self.name
                )));
            }
            validate_relative_path("area folder", folder)?;
        }
        if let Absent::Fallback { path } = &self.absent {
            if !path.is_absolute() {
                return Err(Error::unknown_state(format!(
                    "area {}/{} fallback {} is not absolute",
                    self.namespace,
                    self.name,
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// The fields that differ between this stored declaration and a proposed one,
    /// in a stable order. Empty means the declarations agree.
    pub fn differing_fields(&self, proposed: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.volume != proposed.volume {
            fields.push("volume");
        }
        if self.folder != proposed.folder {
            fields.push("folder");
        }
        if self.absent != proposed.absent {
            fields.push("absent");
        }
        if self.soft_cap_bytes != proposed.soft_cap_bytes {
            fields.push("soft_cap_bytes");
        }
        if self.counts_toward_reserve != proposed.counts_toward_reserve {
            fields.push("counts_toward_reserve");
        }
        fields
    }
}

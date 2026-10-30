//! The identity and absence evidence documents a plan and an intent carry.
//!
//! One document, one definition. A release plan row prints its condition 2 and
//! condition 6 evidence, and a `barge.intent/2` record carries the same two
//! documents so a replay can say what was proved, when, and re-establish it
//! instead of trusting a past verdict (stage 4 design, "Plan and intent").
//! Nothing here is authority: [`IdentityEvidence`] is what a plan observed and
//! [`AbsenceEvidence`] is one `barge.absence/1` proof, and each is re-established
//! before any effect it would let through.
//!
//! Validation is strict and bounded because both documents are read back from a
//! record a human could have edited: a verdict outside the design's set, a reason
//! outside its table, a path that is not absolute with named components only, a
//! `barge.absence/1` `absent` verdict without the ancestor and leaf that prove it,
//! an unknown verdict without a reason, or a document over its 4 KiB bound is
//! unknown state rather than evidence.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::os::VolumeUuid;

/// The `barge.absence/1` schema string.
pub const ABSENCE_SCHEMA: &str = "barge.absence/1";

/// How large one evidence document may be, which is the design's identity and
/// absence bound.
pub const EVIDENCE_CAP_BYTES: usize = 4096;

/// The identity verdicts a row may state.
pub const IDENTITY_VERDICTS: &[&str] = &["verified", "unknown", "gone"];

/// The identity reason codes of the design's reason table, in snake case.
pub const IDENTITY_REASONS: &[&str] = &[
    "volume_unverified",
    "duplicate_volume_uuid",
    "not_a_real_directory",
    "path_changed",
    "inode_changed",
    "marker_absent",
    "marker_unreadable",
    "marker_foreign_owner",
    "marker_version_unknown",
    "marker_schema_divergent",
    "marker_identity_mismatch",
    "marker_generation_unexpected",
    "identity_record_missing",
    "identity_record_invalid",
];

/// The absence verdicts a proof may state.
pub const ABSENCE_VERDICTS: &[&str] = &["absent", "present", "unknown"];

/// The identity evidence one plan row and one intent carry.
///
/// It is the plan's condition 2 evidence: the durable verdict, the reason when it
/// is not verified, the inode and the two devices, the remount and mount-moved
/// facts — which are never refusals on their own — and the marker digest that plan
/// read. A device comparison is diagnostic here; the durable identity is the
/// inode, the marker's own identity and the path below the mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityEvidence {
    /// `verified`, `unknown` or `gone`.
    pub verdict: String,
    /// The reason code, when the verdict is not `verified`. It is one of the
    /// stage 4 design's codes, spelled in snake case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The inode the durable record holds, when the record was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    /// The device the record was written at. It is diagnostic only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_at_record: Option<u64>,
    /// The device the root was observed at in this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_live: Option<u64>,
    /// What the remount changed, which is never a refusal on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remount: Option<RemountEvidenceRow>,
    /// Whether the live mount differs from the recorded one. Also not a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_moved: Option<bool>,
    /// The SHA-256 of the marker bytes this evidence's observation read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_digest: Option<String>,
}

impl IdentityEvidence {
    /// Validate every recorded fact strictly. Unknown is a refusal, never a guess.
    pub fn validate(&self) -> Result<()> {
        if !IDENTITY_VERDICTS.contains(&self.verdict.as_str()) {
            return Err(refuse(format!(
                "`{}` is not an identity verdict",
                self.verdict
            )));
        }
        if let Some(reason) = &self.reason {
            if !IDENTITY_REASONS.contains(&reason.as_str()) {
                return Err(refuse(format!(
                    "`{reason}` is not a stage 4 identity reason code"
                )));
            }
        }
        for (what, value) in [
            ("inode", self.inode),
            ("device_at_record", self.device_at_record),
            ("device_live", self.device_live),
        ] {
            if value == Some(0) {
                return Err(refuse(format!(
                    "the identity evidence records a zero {what}"
                )));
            }
        }
        if let Some(digest) = &self.marker_digest {
            if !is_sha256(digest) {
                return Err(refuse(
                    "the identity evidence's marker digest is not a SHA-256 hex string".to_owned(),
                ));
            }
        }
        // A verified verdict is a complete comparison: the record's inode, both
        // devices, the remount fact, the mount-moved fact and the marker bytes it
        // read. A document that states `verified` without them says nothing a
        // reader could re-establish.
        if self.verdict == "verified" {
            if self.reason.is_some() {
                return Err(refuse(
                    "a verified identity carries no reason code".to_owned(),
                ));
            }
            if self.inode.is_none()
                || self.device_at_record.is_none()
                || self.device_live.is_none()
                || self.remount.is_none()
                || self.mount_moved.is_none()
                || self.marker_digest.is_none()
            {
                return Err(refuse(
                    "a verified identity needs its inode, both devices, the remount and mount-moved facts and the marker digest"
                        .to_owned(),
                ));
            }
        }
        within_cap(self)
    }
}

/// The remount evidence a verified identity reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemountEvidenceRow {
    /// Whether the device at the record differs from the live device.
    pub device_changed: bool,
}

/// One `barge.absence/1` proof, as a plan row and an intent carry it.
///
/// It is the proof's own field set, so a reader of the plan or the intent reads the
/// same document the primitive produced. It authorises nothing: the effect
/// re-establishes it, and a proof is never reused as a permission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceEvidence {
    /// Always `barge.absence/1`.
    pub schema: String,
    /// The consumer path the proof is about.
    pub path: PathBuf,
    /// `absent`, `present` or `unknown`.
    pub verdict: String,
    /// Where the walk started.
    pub anchor: AbsenceAnchor,
    /// The nearest existing ancestor, when the walk reached one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<AbsenceAncestor>,
    /// The leaf, when it was reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf: Option<AbsenceLeaf>,
    /// How many components the walk crossed.
    #[serde(default)]
    pub components_walked: usize,
    /// When the proof was established.
    #[serde(default)]
    pub proved_ms: u64,
    /// The reason code, when the verdict is `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AbsenceEvidence {
    /// Validate every recorded fact strictly. Only a leaf `ENOENT` on a proved
    /// ancestor is `absent`, and only a walk that saw its anchor is `present`.
    pub fn validate(&self) -> Result<()> {
        if self.schema != ABSENCE_SCHEMA {
            return Err(refuse(format!(
                "`{}` is not a `{ABSENCE_SCHEMA}` document",
                self.schema
            )));
        }
        if !ABSENCE_VERDICTS.contains(&self.verdict.as_str()) {
            return Err(refuse(format!(
                "`{}` is not an absence verdict",
                self.verdict
            )));
        }
        named_absolute("path", &self.path)?;
        if !matches!(self.anchor.kind.as_str(), "root" | "registered") {
            return Err(refuse(format!(
                "`{}` is not an absence anchor",
                self.anchor.kind
            )));
        }
        named_absolute("anchor.path", &self.anchor.path)?;
        if let Some(ancestor) = &self.ancestor {
            ancestor.validate()?;
        }
        if let Some(leaf) = &self.leaf {
            if leaf.name.is_empty() || leaf.name.len() > 255 || leaf.name.contains('/') {
                return Err(refuse(
                    "an absence proof's leaf is not one entry name".to_owned(),
                ));
            }
            if leaf.result != "enoent" {
                return Err(refuse(format!(
                    "`{}` is not the leaf result an absence proof records",
                    leaf.result
                )));
            }
        }
        if self.components_walked > 64 {
            return Err(refuse(
                "an absence proof walked more than its 64-component bound".to_owned(),
            ));
        }
        if let Some(reason) = &self.reason {
            // The walk's own codes are [`ABSENCE_REASONS`], but a proof that could
            // not be established at all carries the failure's reason text, so the
            // rule here is a bounded, printable reason and not a closed vocabulary.
            if reason.is_empty() || reason.len() > 160 || reason.chars().any(char::is_control) {
                return Err(refuse(
                    "an absence reason must be 1 to 160 printable bytes".to_owned(),
                ));
            }
        }
        match self.verdict.as_str() {
            // An absent proof names the ancestor it proved on a verified volume and
            // the leaf it found missing. Without either it proves nothing.
            "absent" => {
                if self.ancestor.is_none() || self.leaf.is_none() || self.reason.is_some() {
                    return Err(refuse(
                        "an absent proof needs its ancestor, its `enoent` leaf and no reason"
                            .to_owned(),
                    ));
                }
            }
            // A present walk never reaches a missing leaf, so it has neither.
            "present" => {
                if self.ancestor.is_some() || self.leaf.is_some() || self.reason.is_some() {
                    return Err(refuse(
                        "a present proof walks to the path, so it carries no ancestor, leaf or reason"
                            .to_owned(),
                    ));
                }
            }
            // An undecided walk says why.
            _ => {
                if self.reason.is_none() {
                    return Err(refuse(
                        "an unknown absence needs its reason code".to_owned(),
                    ));
                }
            }
        }
        within_cap(self)
    }
}

/// Where one absence proof walked from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceAnchor {
    /// `root` or `registered`.
    pub kind: String,
    /// The anchor's path.
    pub path: PathBuf,
}

/// The nearest existing ancestor one absence proof verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceAncestor {
    /// The ancestor's path.
    pub path: PathBuf,
    /// Its inode.
    pub inode: u64,
    /// Its device.
    pub device: u64,
    /// The volume it was proved on.
    pub volume: AbsenceVolumeRow,
}

impl AbsenceAncestor {
    /// Validate the ancestor's own facts strictly.
    fn validate(&self) -> Result<()> {
        named_absolute("ancestor.path", &self.path)?;
        if self.inode == 0 || self.device == 0 {
            return Err(refuse(
                "an absence proof's ancestor records a zero inode or device".to_owned(),
            ));
        }
        self.volume.validate()
    }
}

/// The volume an ancestor was proved on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceVolumeRow {
    /// `boot_data` or `registered`.
    pub kind: String,
    /// The APFS volume UUID.
    pub volume_uuid: String,
    /// The filesystem the platform reported.
    pub file_system: String,
}

impl AbsenceVolumeRow {
    /// Validate the volume's own facts strictly.
    fn validate(&self) -> Result<()> {
        if !matches!(self.kind.as_str(), "boot_data" | "registered") {
            return Err(refuse(format!(
                "`{}` is not an absence ancestor volume",
                self.kind
            )));
        }
        let uuid = VolumeUuid::parse(&self.volume_uuid)?;
        if uuid.as_str() != self.volume_uuid {
            return Err(refuse(
                "an absence ancestor's volume UUID is not canonical".to_owned(),
            ));
        }
        if self.file_system.is_empty()
            || self.file_system.len() > 32
            || self.file_system.chars().any(char::is_control)
        {
            return Err(refuse(
                "an absence ancestor records no usable file system".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The leaf one absence proof decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceLeaf {
    /// The leaf's name.
    pub name: String,
    /// What the walk found: `enoent`.
    pub result: String,
}

/// The unknown-state refusal both documents share.
fn refuse(reason: String) -> Error {
    Error::new(ErrorKind::UnknownState, reason)
}

/// Whether a path is absolute and made of named components only.
fn named_absolute(field: &str, path: &Path) -> Result<()> {
    let Some(text) = path.to_str() else {
        return Err(refuse(format!("an evidence `{field}` is not UTF-8")));
    };
    // `/` is the one absolute path with no component, and it is the absence
    // primitive's own anchor for a path proved against the boot Data volume.
    let named = text == "/"
        || (text.starts_with('/')
            && text
                .split('/')
                .skip(1)
                .all(|part| !part.is_empty() && part != "." && part != ".."));
    if !named {
        return Err(refuse(format!(
            "an evidence `{field}` is not an absolute path of named components"
        )));
    }
    Ok(())
}

/// Whether a document is still inside its own 4 KiB bound.
fn within_cap<T: Serialize>(document: &T) -> Result<()> {
    let bytes = serde_json::to_vec(document)
        .map_err(|error| refuse(format!("an evidence document cannot be encoded: {error}")))?;
    if bytes.len() > EVIDENCE_CAP_BYTES {
        return Err(refuse(format!(
            "an evidence document is {} bytes, over the {EVIDENCE_CAP_BYTES}-byte cap",
            bytes.len()
        )));
    }
    Ok(())
}

/// Whether a value is a lowercase SHA-256 hex string.
fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn verified() -> IdentityEvidence {
        IdentityEvidence {
            verdict: "verified".to_owned(),
            reason: None,
            inode: Some(1_234_567),
            device_at_record: Some(16_777_234),
            device_live: Some(16_777_890),
            remount: Some(RemountEvidenceRow {
                device_changed: true,
            }),
            mount_moved: Some(false),
            marker_digest: Some("ab".repeat(32)),
        }
    }

    fn absent() -> AbsenceEvidence {
        AbsenceEvidence {
            schema: ABSENCE_SCHEMA.to_owned(),
            path: PathBuf::from("/Users/tom/Dev/projects/example"),
            verdict: "absent".to_owned(),
            anchor: AbsenceAnchor {
                kind: "root".to_owned(),
                path: PathBuf::from("/"),
            },
            ancestor: Some(AbsenceAncestor {
                path: PathBuf::from("/Users/tom/Dev/projects"),
                inode: 1_234_567,
                device: 16_777_220,
                volume: AbsenceVolumeRow {
                    kind: "boot_data".to_owned(),
                    volume_uuid: "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D".to_owned(),
                    file_system: "apfs".to_owned(),
                },
            }),
            leaf: Some(AbsenceLeaf {
                name: "example".to_owned(),
                result: "enoent".to_owned(),
            }),
            components_walked: 4,
            proved_ms: 0,
            reason: None,
        }
    }

    #[test]
    fn a_verified_identity_and_an_absent_proof_validate() {
        verified().validate().expect("the design's verified row");
        absent().validate().expect("the design's absent proof");
    }

    #[test]
    fn a_verified_identity_needs_its_whole_comparison() {
        let mut evidence = verified();
        evidence.inode = None;
        assert!(
            evidence.validate().is_err(),
            "a verified row needs its inode"
        );
        let mut evidence = verified();
        evidence.reason = Some("inode_changed".to_owned());
        assert!(
            evidence.validate().is_err(),
            "a verified row states no reason"
        );
        let mut evidence = verified();
        evidence.reason = Some("not_a_code".to_owned());
        evidence.verdict = "unknown".to_owned();
        assert!(
            evidence.validate().is_err(),
            "a reason outside the design's table is unknown"
        );
        let mut evidence = verified();
        evidence.marker_digest = Some("ab".repeat(31));
        assert!(
            evidence.validate().is_err(),
            "a marker digest is a full SHA-256 hex string, not a prefix"
        );
        // An unknown row without a reason is what an unobserved plan row states.
        let unobserved = IdentityEvidence {
            verdict: "unknown".to_owned(),
            reason: None,
            inode: None,
            device_at_record: None,
            device_live: None,
            remount: None,
            mount_moved: None,
            marker_digest: None,
        };
        unobserved.validate().expect("an unobserved row is unknown");
    }

    #[test]
    fn an_absent_proof_needs_its_ancestor_leaf_and_volume() {
        let mut proof = absent();
        proof.ancestor = None;
        assert!(
            proof.validate().is_err(),
            "an absent proof needs its ancestor"
        );
        let mut proof = absent();
        proof.leaf = None;
        assert!(proof.validate().is_err(), "an absent proof needs its leaf");
        let mut proof = absent();
        if let Some(leaf) = proof.leaf.as_mut() {
            leaf.result = "present".to_owned();
        }
        assert!(proof.validate().is_err(), "only `enoent` is absence");
        let mut proof = absent();
        if let Some(ancestor) = proof.ancestor.as_mut() {
            ancestor.volume.volume_uuid = "not-a-uuid".to_owned();
        }
        assert!(proof.validate().is_err(), "an ancestor volume is a UUID");
        let mut proof = absent();
        proof.path = PathBuf::from("/Users/tom/../elsewhere");
        assert!(proof.validate().is_err(), "a parent component is refused");
        let mut proof = absent();
        proof.schema = "barge.absence/2".to_owned();
        assert!(proof.validate().is_err(), "an unknown major is refused");
    }

    #[test]
    fn a_present_and_an_unknown_proof_carry_what_their_verdict_says() {
        let present = AbsenceEvidence {
            verdict: "present".to_owned(),
            ancestor: None,
            leaf: None,
            reason: None,
            ..absent()
        };
        present.validate().expect("a present walk carries no leaf");
        let unknown = AbsenceEvidence {
            verdict: "unknown".to_owned(),
            ancestor: None,
            leaf: None,
            reason: Some("ancestor_missing".to_owned()),
            ..absent()
        };
        unknown.validate().expect("an undecided walk says why");
        let missing_reason = AbsenceEvidence {
            reason: None,
            ..unknown.clone()
        };
        assert!(
            missing_reason.validate().is_err(),
            "an undecided walk without a reason is unknown state"
        );
        let bad_reason = AbsenceEvidence {
            reason: Some("because".repeat(64)),
            ..unknown
        };
        assert!(
            bad_reason.validate().is_err(),
            "an over-long reason is refused"
        );
    }
}

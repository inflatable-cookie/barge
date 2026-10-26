//! The versioned-JSON contract: format identities, the document envelopes,
//! the contract bounds and the bounded list shape.
//!
//! Every command prints exactly one JSON document on stdout, and the document
//! names its own format (stage 1 design, "Versioned JSON and error contract").
//! Bounds are part of the contract: a document over its cap is refused with
//! `resource_bound` rather than cut short, and every list carries `count`,
//! `total` and `truncated` so a truncated list is never a silently short one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, ErrorKind, Result};

/// The largest document Barge prints for one command.
///
/// It matches the registry read bound (4 MiB) and sits far above what the row
/// caps below allow, so reaching it is a defect, not the normal truncation path.
pub const DOCUMENT_SIZE_CAP_BYTES: usize = 4 * 1024 * 1024;

/// The row cap for registered volumes (design, "Bounded resources").
pub const VOLUME_ROWS: usize = 64;
/// The row cap for declared areas.
pub const AREA_ROWS: usize = 128;
/// The row cap for placements.
pub const PLACEMENT_ROWS: usize = 4096;
/// The row cap for foreign observations. The design publishes no separate cap,
/// so the largest declared cap is used and a report cannot grow without limit.
pub const OBSERVATION_ROWS: usize = 4096;

/// The describe document's schema string.
pub const DESCRIBE_SCHEMA: &str = "barge.describe/1";
/// The prefix every major of the describe document shares.
pub const DESCRIBE_SCHEMA_PREFIX: &str = "barge.describe/";
/// The error document's schema string.
pub const ERROR_SCHEMA: &str = "barge.error/1";
/// The inspect document's schema string.
pub const INSPECT_SCHEMA: &str = "barge.inspect/1";
/// The prefix every major of the inspect document shares.
pub const INSPECT_SCHEMA_PREFIX: &str = "barge.inspect/";
/// The account volume registry's format (written from slice S2).
pub const REGISTRY_SCHEMA: &str = "barge.registry/1";
/// One area declaration's format.
pub const AREA_SCHEMA: &str = "barge.area/1";
/// One placement record's format.
pub const PLACEMENT_SCHEMA: &str = "barge.placement/1";
/// One owned root's marker format.
pub const ROOT_SCHEMA: &str = "barge.root/1";
/// One hold file's format.
pub const HOLD_SCHEMA: &str = "barge.hold/1";
/// One intent journal record's format.
pub const INTENT_SCHEMA: &str = "barge.intent/1";

/// Every format version Barge speaks, as `describe` reports them. A format's
/// major changes only when its document changes incompatibly; the previous
/// major stays readable for at least one slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatVersions {
    /// `barge.describe/<major>`.
    pub describe: String,
    /// `barge.error/<major>`.
    pub error: String,
    /// `barge.registry/<major>`.
    pub registry: String,
    /// `barge.area/<major>`.
    pub area: String,
    /// `barge.placement/<major>`.
    pub placement: String,
    /// `barge.root/<major>`.
    pub root: String,
    /// `barge.hold/<major>`.
    pub hold: String,
    /// `barge.intent/<major>`.
    pub intent: String,
}

impl FormatVersions {
    /// The formats this build writes and reads.
    pub fn current() -> Self {
        Self {
            describe: DESCRIBE_SCHEMA.to_owned(),
            error: ERROR_SCHEMA.to_owned(),
            registry: REGISTRY_SCHEMA.to_owned(),
            area: AREA_SCHEMA.to_owned(),
            placement: PLACEMENT_SCHEMA.to_owned(),
            root: ROOT_SCHEMA.to_owned(),
            hold: HOLD_SCHEMA.to_owned(),
            intent: INTENT_SCHEMA.to_owned(),
        }
    }
}

/// One bounded list in a document.
///
/// `rows` holds at most the contract's row cap for that list, `count` is what it
/// holds, `total` is what the source held, and `truncated` is true exactly when
/// rows were left out. A reader refuses a set whose numbers disagree with its
/// rows, so a short list can never pass for a complete one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowSet<T> {
    /// The rows reported.
    pub rows: Vec<T>,
    /// How many rows are reported; equal to `rows.len()`.
    pub count: usize,
    /// How many rows the source held.
    pub total: usize,
    /// Whether `total` exceeded the row cap.
    pub truncated: bool,
}

impl<T> RowSet<T> {
    /// A list with no rows.
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            count: 0,
            total: 0,
            truncated: false,
        }
    }

    /// Keep at most `cap` of `rows`, reporting how many the source held. A
    /// caller that undercounts its own rows cannot report a short list as
    /// complete: `total` is never below the rows actually kept.
    pub fn bounded(mut rows: Vec<T>, total: usize, cap: usize) -> Self {
        rows.truncate(cap);
        let count = rows.len();
        let total = total.max(count);
        Self {
            rows,
            count,
            total,
            truncated: total > count,
        }
    }

    /// Refuse counts that disagree with the rows: `count` must equal
    /// `rows.len()`, `total` must cover `count`, and `truncated` must say
    /// exactly whether rows were left out.
    pub fn validate(&self) -> Result<()> {
        let consistent = self.count == self.rows.len()
            && self.total >= self.count
            && self.truncated == (self.total > self.count);
        if consistent {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::UnknownState,
            "a list in the document reports counts that disagree with its rows",
        )
        .with_detail("count", self.count.into())
        .with_detail("rows", self.rows.len().into())
        .with_detail("total", self.total.into())
        .with_detail("truncated", self.truncated.into()))
    }
}

/// A successful command document: schema, command, `ok` and the command's data.
#[derive(Debug, Serialize)]
struct SuccessDocument<'a, T> {
    schema: &'a str,
    command: &'a str,
    ok: bool,
    data: &'a T,
}

/// Encode one successful command document as the single JSON document a caller
/// prints, and refuse it when it exceeds the document size cap.
pub fn success_bytes<T: Serialize>(schema: &str, command: &str, data: &T) -> Result<Vec<u8>> {
    success_bytes_with_cap(schema, command, data, DOCUMENT_SIZE_CAP_BYTES)
}

/// The body of [`success_bytes`] with an explicit cap, so a test can prove the
/// refusal without building a multi-megabyte document.
pub(crate) fn success_bytes_with_cap<T: Serialize>(
    schema: &str,
    command: &str,
    data: &T,
    cap: usize,
) -> Result<Vec<u8>> {
    let document = SuccessDocument {
        schema,
        command,
        ok: true,
        data,
    };
    let mut bytes = serde_json::to_vec(&document).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the {schema} document: {error}"),
        )
    })?;
    if bytes.len() > cap {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the {schema} document is {} bytes, over the {cap}-byte document cap",
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", cap.into()));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Encode the `barge.error/1` document for one failure. The error document is
/// bounded like every other: when its detail would exceed the cap it is
/// replaced by a `resource_bound` document that names the truncation, so a
/// failure always leaves the caller one parseable document.
pub fn error_bytes(command: &str, error: &Error) -> Vec<u8> {
    error_bytes_with_cap(command, error, DOCUMENT_SIZE_CAP_BYTES)
}

/// The body of [`error_bytes`] with an explicit cap.
pub(crate) fn error_bytes_with_cap(command: &str, error: &Error, cap: usize) -> Vec<u8> {
    let full = serde_json::to_vec(&error.document(command))
        .ok()
        .filter(|bytes| bytes.len() <= cap);
    let mut bytes = full.unwrap_or_else(|| {
        let bounded = Error::new(
            ErrorKind::ResourceBound,
            format!("the error document for `{command}` exceeded the {cap}-byte cap; detail omitted"),
        );
        serde_json::to_vec(&bounded.document(command)).unwrap_or_else(|_| {
            br#"{"schema":"barge.error/1","command":"barge","ok":false,"error":{"kind":"io","message":"the error document could not be encoded","detail":{}}}"#.to_vec()
        })
    });
    bytes.push(b'\n');
    bytes
}

/// Parse the JSON value of one document so a reader can check the fields the
/// contract names. Reading is bounded before parsing, never after.
pub(crate) fn parse_document(bytes: &[u8]) -> Result<Value> {
    if bytes.len() > DOCUMENT_SIZE_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the document is {} bytes, over the {DOCUMENT_SIZE_CAP_BYTES}-byte document cap",
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", DOCUMENT_SIZE_CAP_BYTES.into()));
    }
    serde_json::from_slice(bytes).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!("the document is not one JSON document: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        error_bytes_with_cap, success_bytes_with_cap, FormatVersions, RowSet,
        DOCUMENT_SIZE_CAP_BYTES,
    };
    use crate::error::{Error, ErrorKind};
    use serde_json::{json, Value};

    #[test]
    fn the_format_versions_are_the_ones_the_contract_names() {
        let formats = FormatVersions::current();
        assert_eq!(formats.describe, "barge.describe/1");
        assert_eq!(formats.error, "barge.error/1");
        assert_eq!(formats.registry, "barge.registry/1");
        assert_eq!(formats.area, "barge.area/1");
        assert_eq!(formats.placement, "barge.placement/1");
        assert_eq!(formats.root, "barge.root/1");
        assert_eq!(formats.hold, "barge.hold/1");
        assert_eq!(formats.intent, "barge.intent/1");
    }

    #[test]
    fn a_successful_document_refuses_the_size_cap() {
        let data = json!({"message": "x".repeat(64)});
        let error = success_bytes_with_cap("barge.test/1", "test", &data, 16)
            .expect_err("an oversized document is refused");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
        assert_eq!(error.detail()["cap"], 16);
        assert!(error.detail()["bytes"].as_u64().unwrap_or_default() > 16);
    }

    #[test]
    fn a_successful_document_ends_with_one_newline() {
        let bytes = success_bytes_with_cap("barge.test/1", "test", &json!({"a": 1}), 4096)
            .expect("a small document encodes");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert!(bytes.ends_with(b"\n"));
    }

    #[test]
    fn an_error_document_stays_parseable_when_it_exceeds_the_cap() {
        let error = Error::new(ErrorKind::Io, "x".repeat(4096));
        let bytes = error_bytes_with_cap("describe", &error, 512);
        assert!(bytes.len() <= 512 + 1, "the fallback fits the cap");
        let document: Value = serde_json::from_slice(&bytes).expect("the fallback parses");
        assert_eq!(document["schema"], "barge.error/1");
        assert_eq!(document["ok"], false);
        assert_eq!(document["error"]["kind"], "resource_bound");
    }

    #[test]
    fn an_error_document_keeps_its_kind_when_it_fits() {
        let error = Error::new(ErrorKind::VolumeAbsent, "the drive is not attached");
        let document: Value =
            serde_json::from_slice(&error_bytes_with_cap("describe", &error, 4096))
                .expect("the document parses");
        assert_eq!(document["error"]["kind"], "volume_absent");
        assert_eq!(document["command"], "describe");
    }

    #[test]
    fn the_document_size_cap_is_the_registry_read_bound() {
        assert_eq!(DOCUMENT_SIZE_CAP_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn a_truncated_list_keeps_its_counts() {
        let rows = RowSet::bounded(vec![1, 2, 3], 5, 2);
        assert_eq!(rows.rows, vec![1, 2]);
        assert_eq!(rows.count, 2);
        assert_eq!(rows.total, 5);
        assert!(rows.truncated);
        rows.validate().expect("truncation is consistent");
    }

    #[test]
    fn a_complete_list_is_not_truncated() {
        let rows = RowSet::bounded(vec![1, 2], 2, 8);
        assert_eq!(rows.count, 2);
        assert_eq!(rows.total, 2);
        assert!(!rows.truncated);
        rows.validate().expect("a complete list is consistent");
    }

    #[test]
    fn an_undercounted_source_cannot_report_a_short_list_as_complete() {
        let rows = RowSet::bounded(vec![1, 2, 3], 1, 8);
        assert_eq!(rows.count, 3);
        assert_eq!(rows.total, 3);
        assert!(!rows.truncated);
        rows.validate().expect("total is raised to the kept rows");
    }

    #[test]
    fn a_list_whose_counts_disagree_is_refused() {
        let mut rows: RowSet<String> = RowSet::empty();
        rows.total = 3;
        let error = rows
            .validate()
            .expect_err("a short list cannot claim three");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        let mut rows: RowSet<String> = RowSet::empty();
        rows.count = 1;
        assert!(rows.validate().is_err());
    }
}

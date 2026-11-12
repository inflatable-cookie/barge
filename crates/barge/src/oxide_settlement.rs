//! Oxide's coordinated-registry settlement set: the one strict wire reading
//! both of Barge's registry readers share.
//!
//! `oxide.outputs.registry/4` (Oxide's single-authority adoption, read
//! read-only at Oxide's own published source) requires a present, bounded
//! `settled` array, whose entries are retained terminal domain facts: the
//! atomic record that one delegated deletion removed its exact group. A
//! settlement is never a live root reference, never deletion authority and
//! never a second progress journal — the registry's current rows and intents
//! still decide everything a reference depends on, even where a settlement
//! names the same path. That is why this module validates and drops: a
//! settlement that fails its own strict rules makes the whole document
//! divergent (unknown, batch kept), and one that passes contributes no
//! reference at all.
//!
//! Both readers ([`crate::reference`] for the release gate and
//! [`crate::census`] for `barge inspect`) deserialize and validate through
//! this one module, so they cannot disagree about which `/4` documents are
//! valid. Each reader keeps its own error wording and its own historical
//! version set.

use std::path::PathBuf;

use serde::Deserialize;

use crate::reference::{is_hex32, is_hex64, is_safe_absolute};

/// Oxide's own cap: the retained settlement set shares the prune intents'
/// 256-entry cap in Oxide's `Registry::validate`.
pub(crate) const OXIDE_SETTLED_CAP: usize = 256;

/// The caller name every settlement names. Oxide's adapter publishes its
/// settlements under exactly this caller.
pub(crate) const OXIDE_SETTLED_CALLER: &str = "oxide.outputs";

/// The only outcome a retained settlement records: the group is gone.
pub(crate) const OXIDE_SETTLED_OUTCOME: &str = "deleted";

/// One settlement entry, exactly the field set Oxide writes.
///
/// Every member is required and `deny_unknown_fields` keeps the entry exact.
/// `nonce` is an absent-able member on the wire, but Oxide's own validation
/// requires it present and well-formed, and this reader matches that.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSettlement {
    #[allow(dead_code)]
    caller: String,
    #[allow(dead_code)]
    key: String,
    #[allow(dead_code)]
    checkout_id: String,
    #[allow(dead_code)]
    root: PathBuf,
    #[allow(dead_code)]
    nonce: Option<String>,
    #[allow(dead_code)]
    outcome: String,
    #[allow(dead_code)]
    terminal_ms: u64,
    #[allow(dead_code)]
    request_sha256: String,
}

/// An explicitly present `settled` member must be an array, including when
/// empty.
///
/// Like Oxide's own reader, this accepts `[]` and refuses an explicit `null`:
/// null is not a missing predecessor field, and on `/4` a missing member is
/// refused in the parse, so neither ever reads as an empty set.
pub(crate) fn read_settlements<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<RawSettlement>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::deserialize(deserializer).map(Some)
}

/// Oxide's own settlement validation, as one shared rule.
///
/// A settlement names its caller (`oxide.outputs`), a lowercase 32-hex key, a
/// lowercase 64-hex checkout id, an absolute usable root, a present lowercase
/// 32-hex nonce, the one `deleted` outcome, a positive terminal time and a
/// lowercase 64-hex request digest; `(caller, key)` is unique and the set is
/// capped. Anything else makes the whole document divergent.
pub(crate) fn validate_settlement_set(entries: &[RawSettlement]) -> Result<(), String> {
    if entries.len() > OXIDE_SETTLED_CAP {
        return Err(format!(
            "the retained settlement set holds {} entries, over the {OXIDE_SETTLED_CAP} Oxide retains",
            entries.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        if entry.caller != OXIDE_SETTLED_CALLER
            || !is_hex32(&entry.key)
            || !is_hex64(&entry.checkout_id)
            || !is_safe_absolute(&entry.root)
            || !entry.nonce.as_deref().is_some_and(is_hex32)
            || entry.outcome != OXIDE_SETTLED_OUTCOME
            || entry.terminal_ms == 0
            || !is_hex64(&entry.request_sha256)
            || !seen.insert((&entry.caller, &entry.key))
        {
            return Err("the retained settlement set is invalid or repeated".to_owned());
        }
    }
    Ok(())
}

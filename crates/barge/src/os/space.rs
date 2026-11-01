//! Space measurements from a verified volume, with checked block arithmetic.

use serde::{Deserialize, Serialize};

use super::{DeviceId, VerifiedVolume};
use crate::error::{Error, ErrorKind, Result};

/// A kernel measurement and its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceMeasurement {
    /// Bytes available to an unprivileged writer.
    pub free_bytes: u64,
    /// Free bytes including blocks reserved from unprivileged writers.
    pub free_blocks_total_bytes: u64,
    /// Total filesystem bytes.
    pub total_bytes: u64,
    /// Kernel field used for the decision value.
    pub source: String,
    /// APFS purgeable space cannot be measured separately.
    pub purgeable: String,
    /// Completion time in milliseconds since the epoch.
    pub observed_ms: u64,
    /// Device on which the descriptor was opened.
    pub device: DeviceId,
}

impl SpaceMeasurement {
    /// Convert kernel block counts without substituting for unreadable values.
    pub fn from_blocks(
        block_size: Option<u64>,
        available: Option<u64>,
        free: Option<u64>,
        total: Option<u64>,
        observed_ms: u64,
        device: DeviceId,
    ) -> Result<Self> {
        let block_size = block_size.filter(|size| *size != 0);
        let bytes = |blocks: Option<u64>| {
            block_size
                .and_then(|size| blocks.and_then(|count| size.checked_mul(count)))
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::UnknownState,
                        "space block count is unreadable or overflows",
                    )
                })
        };
        Ok(Self {
            free_bytes: bytes(available)?,
            free_blocks_total_bytes: bytes(free)?,
            total_bytes: bytes(total)?,
            source: "fstatfs.f_bavail*bsize".to_owned(),
            purgeable: "unknown".to_owned(),
            observed_ms,
            device,
        })
    }
}

/// Read a measurement from a reverified volume.
pub trait SpaceFacts {
    /// Refuse unreadable facts and arithmetic overflow as unknown state.
    fn measure_space(&self, volume: &VerifiedVolume) -> Result<SpaceMeasurement>;
}

/// Flush a reverified volume after a deletion batch.
pub trait Flush {
    /// Enforce the absolute deadline while flushing. On timeout kill the owned
    /// helper and positively reap it within a one-second cleanup bound; report
    /// uncertain custody if that cannot be established. Never infer reclaim.
    fn flush_volume(&self, volume: &VerifiedVolume, deadline: std::time::Instant) -> Result<()>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn checked_blocks_preserve_provenance_and_unknown_purgeable() {
        let m = SpaceMeasurement::from_blocks(
            Some(4096),
            Some(2),
            Some(3),
            Some(10),
            42,
            DeviceId::new(7),
        )
        .expect("valid counts");
        assert_eq!(
            (m.free_bytes, m.free_blocks_total_bytes, m.total_bytes),
            (8192, 12288, 40960)
        );
        assert_eq!(
            (
                m.source.as_str(),
                m.purgeable.as_str(),
                m.observed_ms,
                m.device.get()
            ),
            ("fstatfs.f_bavail*bsize", "unknown", 42, 7)
        );
        for value in [
            SpaceMeasurement::from_blocks(
                Some(u64::MAX),
                Some(2),
                Some(1),
                Some(1),
                0,
                DeviceId::new(1),
            ),
            SpaceMeasurement::from_blocks(Some(1), None, Some(1), Some(1), 0, DeviceId::new(1)),
        ] {
            assert_eq!(
                value.expect_err("unknown fact").kind(),
                ErrorKind::UnknownState
            );
        }
    }
}

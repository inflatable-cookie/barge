//! Barge's library: every mechanic, no argv.
//!
//! The CLI (`crates/barge-cli`) is a thin shell over this crate, and Oxide links
//! the library directly. Module ownership follows the stage 1 design's module
//! table (`docs/knowledge/contracts/stage1-design.md`, "Crate and CLI
//! skeleton"). Slice S1 implements `os`, `describe` and the shared error and
//! JSON contract; every other module states its job and carries no placeholder
//! logic for a later slice to undo.
//!
//! `unsafe` is denied for the whole crate and allowed only in the macOS
//! implementation of the [`os`] layer, which is the one FFI seam.

#![deny(unsafe_code)]
// RUST-ERR-001: a recoverable failure is a `Result` in the shipped path. Test
// modules opt out deliberately, where a failed assertion is the point.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod area;
pub mod cache;
pub mod contract;
pub mod custody;
pub mod delete;
pub mod describe;
pub mod error;
pub mod inspect;
pub mod os;
pub mod prune;
pub mod registry;
pub mod scratch;
pub mod state;

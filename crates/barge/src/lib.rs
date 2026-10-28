//! Barge's library: every mechanic, no argv.
//!
//! The CLI (`crates/barge-cli`) is a thin shell over this crate, and Oxide links
//! the library directly. Module ownership follows the stage 1 design's module
//! table (`docs/knowledge/contracts/stage1-design.md`, "Crate and CLI
//! skeleton"). Slice S1 implements `os`, `describe` and the shared error and
//! JSON contract. Slice S2 adds the account state directory with its atomic
//! publish, bounded reads and fixed lock order, the `barge.registry/1` reader
//! and its fixture-tested writer, the read-only Oxide observations and
//! `barge inspect`. Slice S3 adds `area`: path containment and the descriptor
//! walk, `barge.area/1` declarations, owned roots and their versioned root
//! markers, cooperative holds and bounded root listing, with `barge area` verbs
//! as the CLI's form of them. Slice S6 adds `cache`: the four cache kinds, the
//! three placement forms, and adopt and undo with their byte-level and
//! link/root identity proofs, with `barge cache` verbs as the CLI's form. Slice
//! S7 adds `prune`: `barge maintain` in preview and real form, which sweeps every
//! registered, present volume's scratch folder through S4's sweep and then
//! applies each declared area's prune policy, with one canonical plan and digest
//! and the 0/2/3 exit contract. Every other module states its job and carries no
//! placeholder logic for a later slice to undo.
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
pub mod diagnostic;
pub mod digest;
pub mod error;
pub mod identity;
pub mod inspect;
pub mod os;
pub mod prune;
pub mod registry;
pub mod scratch;
pub mod space;
pub mod state;
pub mod unit;
pub mod volume;

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod testkit;

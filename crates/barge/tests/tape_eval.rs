#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The F6-O2 B-S1 capture harness executable.
//!
//! This target compiles the library's own sources through `#[path]` includes
//! with `cfg(test)` set (proven reachable: a `harness = false` test target is
//! compiled with `--test`, so `cfg(test)` holds here and every
//! `#[cfg(test)]`-attributed leaf statement in the included sources is active).
//! It is the only build where the leaf tape is live alongside the actual
//! `release::plan`; no production build compiles either, and the linked
//! library rlib is never used for instrumented code. Nothing here is public,
//! no feature is enabled, and the production dependency Oxide pins is
//! untouched.
//!
//! `cache.rs` and `space.rs` declare child modules, and a `#[path]`-included
//! file's children resolve beside the file itself; the symlink shadows at
//! `src/{undo_capture,effect,guard}.rs` point back at the real child files, so
//! every byte compiled here is the library's own source, compiled once, with
//! the real `super::` parents. `lib.rs` never declares those names, so a
//! production build compiles none of them.

// The included library sources carry their own `#[cfg(test)]` unit-test
// modules. A `harness = false` target never collects their `#[test]`
// functions, so their helpers and imports read as dead here; the library's
// own suite (effigy rust:test) is what runs them.
#![allow(dead_code, unused_imports)]
//!
//! ## Argv contract (fixed by the accepted capture contract)
//!
//! ```text
//! tape_eval --fixture-root <dir> --manifest <file> --transcript-out <file> --case <name>
//! ```
//!
//! The fixture root is an exclusively owned directory tree on a real selected
//! volume; the manifest is one `oxide.capture.manifest/1` document; the
//! transcript is always one `oxide.capture.transcript/1` document, including
//! on the engine's refusal. The `--case` value must equal the manifest's
//! `case` member, so a mismatched invocation is refused before any window.
//!
//! Exit codes: the engine's own — `0` or `3` for a completed plan
//! (`PruneReport::needs_human`), `2` or `3` for a refusal
//! (`ErrorKind::exit_code`). An apparatus error — missing arguments, an
//! unreadable manifest or setup failure — exits `2` with the reason on stderr
//! and attempts an apparatus refusal document once the output path is known.
//! An unwritable output cannot carry a document, and missing output arguments
//! do not name a destination. Neither case has an engine outcome.
//!
//! ## Embedded end-to-end exercise
//!
//! With no arguments, the harness runs its embedded exercise: it builds one
//! exclusively owned directory fixture under this invocation's run root
//! (AGENTS.md scratch), writes its manifest, runs the same [`run`] path, and
//! asserts the transcript's shape. `oxide test --test tape_eval` selects it.
//!
//! ## Staging for the later X-S2 orchestration
//!
//! Build from the exact staged commit offline, default features only, pinned
//! toolchain 1.97.1, committed `Cargo.lock`:
//!
//! `effigy rust:test:tape-stage` declares the locked default-feature build;
//! verified export and provenance remain to be supplied.
//!
//! The produced executable under the managed target directory is the one a
//! window invokes; its digest and the toolchain are the orchestrator's to
//! verify before any run. No fixture, pool, registry, root or account outside
//! the given fixture root is read or written.

#[path = "../src/area.rs"]
mod area;
#[path = "../src/cache.rs"]
mod cache;
#[path = "../src/census.rs"]
mod census;
#[path = "../src/contract.rs"]
mod contract;
#[path = "../src/custody.rs"]
mod custody;
#[path = "../src/delete.rs"]
mod delete;
#[path = "../src/describe.rs"]
mod describe;
#[path = "../src/diagnostic.rs"]
mod diagnostic;
#[path = "../src/digest.rs"]
mod digest;
#[path = "../src/error.rs"]
mod error;
#[path = "../src/evidence.rs"]
mod evidence;
#[path = "../src/identity.rs"]
mod identity;
#[path = "../src/inspect.rs"]
mod inspect;
#[path = "../src/os/mod.rs"]
mod os;
#[path = "../src/pool_custody.rs"]
mod pool_custody;
#[path = "../src/prune.rs"]
mod prune;
#[path = "../src/reference.rs"]
mod reference;
#[path = "../src/registry.rs"]
mod registry;
#[path = "../src/release.rs"]
mod release;
#[path = "../src/scratch.rs"]
mod scratch;
#[path = "../src/space.rs"]
mod space;
#[path = "../src/state.rs"]
mod state;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../src/tape.rs"]
mod tape;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../src/tape_harness.rs"]
mod tape_harness;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../src/testkit.rs"]
mod testkit;
#[path = "../src/unit.rs"]
mod unit;
#[path = "../src/volume.rs"]
mod volume;

/// One parsed invocation.
struct Invocation {
    fixture_root: std::path::PathBuf,
    manifest: std::path::PathBuf,
    transcript_out: std::path::PathBuf,
    case: String,
}

/// Parse the fixed argv contract, or return the usage reason.
fn parse_args() -> Result<Invocation, String> {
    let mut fixture_root = None;
    let mut manifest = None;
    let mut transcript_out = None;
    let mut case = None;
    let mut args = std::env::args().skip(1);
    while let Some(name) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("--{name} needs a value"));
        match name.as_str() {
            "--fixture-root" => fixture_root = Some(value("fixture-root")?),
            "--manifest" => manifest = Some(value("manifest")?),
            "--transcript-out" => transcript_out = Some(value("transcript-out")?),
            "--case" => case = Some(value("case")?),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Invocation {
        fixture_root: fixture_root
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "--fixture-root is required".to_owned())?,
        manifest: manifest
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "--manifest is required".to_owned())?,
        transcript_out: transcript_out
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "--transcript-out is required".to_owned())?,
        case: case.ok_or_else(|| "--case is required".to_owned())?,
    })
}

fn main() -> std::process::ExitCode {
    // No arguments: the embedded end-to-end exercise over its own fixture.
    if std::env::args().count() == 1 {
        return match tape_harness::embedded_exercise() {
            Ok(summary) => std::process::ExitCode::from(summary.exit_code),
            Err(reason) => {
                eprintln!("barge.tape.harness/1: the embedded exercise failed: {reason}");
                std::process::ExitCode::from(2)
            }
        };
    }
    let invocation = match parse_args() {
        Ok(invocation) => invocation,
        Err(reason) => {
            eprintln!(
                "barge.tape.harness/1: {reason}; the argv contract is \
                 --fixture-root <dir> --manifest <file> --transcript-out <file> --case <name>"
            );
            return std::process::ExitCode::from(2);
        }
    };
    let manifest_bytes = match std::fs::read(&invocation.manifest) {
        Ok(bytes) => bytes,
        Err(error) => {
            let reason = format!("the manifest cannot be read: {error}");
            if let Err(write_error) = tape_harness::write_apparatus_refusal(
                &invocation.transcript_out,
                None,
                &reason,
            ) {
                eprintln!("barge.tape.harness/1: {write_error}");
            }
            eprintln!("barge.tape.harness/1: {reason}");
            return std::process::ExitCode::from(2);
        }
    };
    match tape_harness::parse_manifest(&manifest_bytes) {
        Ok(request) if request.case == invocation.case => {}
        Ok(request) => {
            let reason = format!(
                "--case {} does not match the manifest's case {}",
                invocation.case, request.case
            );
            if let Err(write_error) = tape_harness::write_apparatus_refusal(
                &invocation.transcript_out,
                Some(&manifest_bytes),
                &reason,
            ) {
                eprintln!("barge.tape.harness/1: {write_error}");
            }
            eprintln!(
                "barge.tape.harness/1: --case {} does not match the manifest's case {}",
                invocation.case, request.case
            );
            return std::process::ExitCode::from(2);
        }
        Err(reason) => {
            if let Err(write_error) = tape_harness::write_apparatus_refusal(
                &invocation.transcript_out,
                Some(&manifest_bytes),
                &reason,
            ) {
                eprintln!("barge.tape.harness/1: {write_error}");
            }
            eprintln!("barge.tape.harness/1: {reason}");
            return std::process::ExitCode::from(2);
        }
    }
    match tape_harness::run(
        &manifest_bytes,
        &invocation.fixture_root,
        &invocation.transcript_out,
    ) {
        Ok(summary) => std::process::ExitCode::from(summary.exit_code),
        Err(reason) => {
            eprintln!("barge.tape.harness/1: {reason}");
            std::process::ExitCode::from(2)
        }
    }
}

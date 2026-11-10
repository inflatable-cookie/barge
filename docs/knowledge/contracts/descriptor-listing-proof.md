# Targeted shared-listing kernel proof

The [shared listing](stage1-design.md#bounded-shared-descriptor-listing-2026-11-10)
has a narrowly targeted native apparatus in
[descriptor-listing-proof.py](../../../scripts/native/descriptor-listing-proof.py).
Its intended claim is actual kernel listing and per-descriptor observation of
one owned child. It does not qualify a whole-host custody scan, integration, deletion,
production account lookup, installation, adoption or lifetime closure.

## Entry and source binding

`effigy listing:native:test` runs tiny-file controls with fake external
executors. `effigy listing:native:plan` reads only this caller's own
`RLIMIT_NOFILE` and prints the fixed plan. Neither starts a native or stages a
binary. The native and diagnostic selectors remain **held for separate owner
activation**. Declarations and ordinary covering are no activation.
`effigy rust:compile:listing:proof` checks the driver with only its proof
feature. `effigy rust:test:listing:proof` drives the actual endpoint's guard,
lossless inventory and encoded-size controls, without a process read.

Each future occurrence verifies the qualified supervisor's exact SHA-256
`fcfda2b0a43afd192a4f75e0d575b9aa94f0fd79d195bd75b1c66305f37f046b` and Stash
UUID `8A660440-C919-45C6-A3F1-3365B9BD2006`, then enters that guard at
300 s / 512 MiB under `/Volumes/Stash/scratch/barge`. No image is used.
Source must be a clean exact Git head. The accepted release staging helper
builds a fresh `descriptor_listing_proof` example inside this guarded
occurrence through locked/offline Oxide, with pinned toolchain `1.97.1` and
only `proof-descriptor-listing`. The manifest binds source head/tree,
lock/toolchain/script hashes and binary bytes/size/features; source is checked
again after staging. No separate stage, prewarm, old binary or shared copy is
accepted. The ordinary release build contains no endpoint.

The endpoint accepts only the named owned child PID, a root in this fixture,
and the current guard's verified scratch/evidence layout. It calls the actual
private listing helper and per-fd observer. Its at-most-64-KiB report includes
exact `fd_numbers` and `fd_type_counts` from that helper's accepted final rows,
plus the exact `matching_fd_numbers` observed inside the target. A bounded
listing has no final inventory; it does not report an empty complete set.
The default consumer scan remains real and unfiltered. The endpoint's
`safe_sample` label describes the targeted sample and is never a public `Safe`
custody value.

## Fixed cases and bounds

| Case | Owned fixture and deciding evidence |
| --- | --- |
| L1 | A dedicated holder opens 32 actual payload descriptors after establishing its exact descriptor set. A single complete listing outside the target reports exactly that declared set and no matching descriptor. |
| L2 | A new dedicated holder opens 5000 actual payload descriptors under the same construction. The kernel's actual total must be 4097–8191 and exactly match the declared set. A full 4096-row attempt grows to 8192 and completes outside the target. |
| L3 | The same large child, PID/start/effective UID unchanged, is observed with the held file inside the target. Actual per-fd observation finds all 5000 declared payload descriptors inside and reports unsafe. |
| L4 | The same large child is listed with only 4096 returned-row work available. The full answer is bounded and custody remains unknown. |

Each report retains every attempt's capacity/returned rows and before/after
kernel identity, the final descriptor numbers and type counts, final count,
total returned-row work and deadline bound. Discarded full prefixes count in
work. The oracle compares the observed descriptor set exactly with the
holder's declaration; neither a fixed `+3`/`+4` allowance nor a newly observed
extra row supplies an expected value. Missing, duplicate or additional rows
fail, and the exact type counts must sum to the final count. L3's matching set
must equal the declared payload set; outside cases require an empty matching
set. Only the final
complete listing is evaluated. No simulated process table
or synthetic observer supplies kernel credit.

The dedicated holder starts with `close_fds=True`. After imports, it uses
only its own supported finite soft `RLIMIT_NOFILE` as the close range
`3..soft-1`, before opening its fixed 32 or 5000 payload descriptors. Each
close must succeed or report `EBADF`; any other answer refuses. Standard
streams must positively be fds 0/1/2: stdin a FIFO, stdout/stderr the same
owned regular log, with actual kind/device/inode/uid facts. The parent verifies
the log binding. The holder declares the exact unique payload fd numbers and
those stream facts only after its temporary readiness-marker fd is closed.
It writes the bounded declaration through existing stdout, flushes, then
renames the readiness marker. There is no open readiness fd in the declared
set.

Readiness and execution require the existing soft limit to be an integer
from 5064 (5000 opens plus the unchanged 64-fd margin) through 1,048,576.
Infinity, an unsupported or unrepresentable value, inadequate headroom,
failed stream facts or uncertain closure refuse. Only the holder's own
nonstandard descriptors are closed; no unrelated process or host-wide limit
is surveyed. There is no limit raising or automatic retry. Readiness is a
prerequisite check, not a resource-fit promise.
Two holders attempt at most 2,097,146 baseline closes in total. Each keeps the
existing five-second readiness window; setup fit is unmeasured and no timing
experiment is part of this source correction.

The finite proposal reserves 0–210 s for fresh staging including waits,
210–255 s for two sequential holders and four inspections, 255–270 s for
holder closure and receipt, and 30 s for the guard's group closure. All phases
spend the original guard deadline; these ranges are estimates, not measured
fit. No work starts after deadline minus 30 s. Each inspection is at most
10 s, its report at most 64 KiB. The fixture plans less than 1 MiB plus the
one staged binary/manifest, whose actual size is recorded. Compiler outputs
retain ordinary Oxide pool placement; compiler temporaries use guard scratch.
The 512 MiB guard budget is monitored scratch, not a hard RSS claim.

## Premises and helper composition

The nine-premise checklist is the pinned read-only Oxide
`docs/knowledge/contracts/workflow.md`, “Proof-authoring premise checklist.”
The executable `premise_dispositions()` publishes the same map:

| Premise | Actual source/disposition |
| --- | --- |
| Private HOME/rustup | `driver_environment` captures invoking rustup home before private HOME/CARGO_HOME. The ordinary staging build preserves its account homes, uses the committed lock, pins `1.97.1`, disables auto-install and stays offline. No fixture compilation. |
| Account control | `driver_environment` explicitly sets `OXIDE_TEST_HOME_FROM_ENV=1`, private `BARGE_ACCOUNT_HOME` and state. The targeted endpoint needs no account/store lookup. |
| Inherited environment | `driver_environment` scrubs inherited Oxide/XDG/Barge/Cargo/rustup state before explicit fixture bindings. Only the verified guard layout/deadline is restored. `outer_half` preserves genuine Nucleus/Effigy admission context. |
| Automatic adoption | No fixture checkout build or adoption. The staging build follows ordinary accepted Oxide output placement. |
| External Cargo argv | Not applicable: no fake external subcommand. The accepted build argv invokes Oxide directly. |
| Image ownership | Not applicable: no image operations. |
| Compiler flags | The imported release `stage_environment` removes both Rust flag forms and the interposer; the wrapper also binds TMP/TEMP. Driver children scrub the same forms. |
| Selected scratch/TMP | Every created fixture/stage directory has an explicit fresh owned parent. All three temporary variables agree. Holder normalization closes only that child's own nonstandard descriptors within its finite supported soft limit; standard streams and the exact post-readiness payload set are positively declared. |
| macOS mktemp | No shell `mktemp`; every `tempfile.mkdtemp` has `dir=`. |

`DirectChildren` supplies only the receiver fields required by the existing
`space_images.Images.run`: its actual `CLOCK_UPTIME_RAW` clock, direct-child
tracking, communication, reap and timeout/unknown handling are reused without
image construction. Staging reuses `release-proof.py`'s allocation, copying,
manifest, environment and bounds through its declared command, feature and
artifact receiver globals. The receiver's monotonic staging deadline is
derived conservatively from remaining raw-uptime duration; unlike-clock
stamps are never subtracted or passed to it. The endpoint receives relative
remaining milliseconds, never a cross-process timestamp.

The effect-free test drives the actual unguarded-entry refusal, staging helper
with tiny artifact/fake compiler output, flag/temporary environment, actual
imported child helper including its timeout custody, endpoint argv/report
adapter, exact descriptor-set/type-count and identity/work oracles, holder
normalization/refusal branches and receipt publication failures. It runs no
kernel proof, child holder or build.

## Receipts and remaining gates

The script writes `descriptor-listing-proof-receipt.json` in the current
guard's evidence directory. It records source/stage bindings, each actual
case/report/error or omission, holders' ordinary EOF closure/reap, resources
and targeted child closure. The supervisor's separate receipt owns group
closure and measured resource/timeout conclusions. Both are required.

A diagnostic is always uncounted and `qualified:false`. It continues only
after a safely classified oracle mismatch; unreadable reports, changed
identity, deadline, infrastructure or unknown child custody stop it and leave
unreached cases unexecuted. A counted run stops at its first fault. Missing
receipts and unexecuted cases are missing proof.

The first diagnostic at source
`43f8bb4d2c5ee8c87fcdb2c64c50e773cbf1ee34` failed the descriptor-count
premise: its actual listing did not match the holder's assumed total. The
historical extra row's cause remains **unknown**. Deterministic holder
construction and exact-set evidence repair the source premise; they neither
explain that observation nor qualify it retroactively. Separately, that run's
UNCLOSED group outcome lacked a completed, joined census. Fixture correction
does not discharge that process-custody obligation. The failed receipt,
unknowns, original bounds and retained resources remain historical.

This source correction runs no staging, holder or native occurrence and
supplies no new kernel or closure credit. Independent exact-head
source/readiness review, an adequate separately activated uncounted diagnostic,
separately activated counted author and
distinct retained-reviewer occurrences, and final exact-head judgment remain
required before the implementation's merge hold can close. Targeted receipts
give no whole-host or Oxide integrated-acceptance credit.

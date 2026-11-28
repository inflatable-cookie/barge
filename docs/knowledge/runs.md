# Platform runs

An initiator supplies a platform constraint, minimum capabilities, source
identity, an argument vector, explicit environment entries, a wall-clock budget,
a fallback policy, and an owner. The owner identifies a worktree lease or an
absolute checkout path.

Barge resolves the request. The initiator executes local work and records the
result. A remote resolution reserves capacity for the initiator. Waking the
machine and executing remote work are later phases.

## Targets

The operator puts `run-targets.json` in the account state directory beside
[`container-profiles.json`](containers.md#profiles).

```json
{
  "local_container": [
    {"profile": "apple-container", "platforms": ["linux-arm64", "linux-any"]}
  ],
  "remote": [
    {"machine": "bazzite", "platform": "linux-x86_64", "max": 2,
     "capabilities": {"cores": 16, "memory_gib": 30, "gpu": true, "vram_gib": 8}}
  ]
}
```

Both lists are optional. With no targets file, only the native host can match.
Each machine has one entry and a concrete `os-arch` platform. Use its
registry identifier verbatim for the target and the hold commands. The library
treats it as an opaque key and uses only the facts in this file. Omitted
capabilities default to zero or false. `memory_gib` and `vram_gib` are whole
GiB. Positive VRAM requirements also require `gpu: true`. Each lease uses one
of the machine's `max` slots.

An `os-arch` entry can serve a matching exact, OS-only, or unrestricted
request. Local container profiles also accept `os-any` for OS-only and
unrestricted requests. `arm64` and `aarch64` are equivalent, as are `amd64`
and `x86_64`.

## Resolution order

1. A remote machine that matches the platform and capabilities and has a free
   slot. Sleeping machines are eligible. Slots last reserved for the same
   repository are preferred across machines. Barge breaks ties by declaration
   order, then slot number.
2. With local fallback allowed, the native host if it matches. The running
   binary supplies the OS and architecture. Barge compares minimum cores
   with parallelism available to the process and minimum memory with total
   host memory. An unavailable measurement cannot meet a positive minimum.
3. The first matching managed local container profile in file order.
   Container placement uses the declared platform and worker capacity.
4. Denied. A remote-only request stops after the remote attempt. Both local
   paths leave GPU and positive VRAM requirements unmet.

When all eligible unheld machines are full, `run_resolve` waits up to the
requested capacity wait before trying local fallback. If all matching
machines are held, the resolver can fall back immediately. A local container
can use any capacity wait left after the remote attempt. Callers can receive
advisory positions among runs waiting for the same machine or profile in the
same process. Position starts at one and can change as other callers leave.
All processes compete for capacity through the account lock.

`barge run resolve --json < request.json` previews immediate placement. A
remote preview accounts for current slots and holds. A container preview
names the profile. Preview takes no lease and does not wait. See the
[JSON contract](contracts/consumption.md#run-documents).

## Machine leases

`Resolution::Remote` supplies a held `MachineLease` with an ID, machine and
zero-based slot. The build area on that machine is keyed by repository and
slot. The account records the last repository assigned to each slot, so a
returned slot can be preferred on a later run. Leases, repository assignments
and operator holds are stored in `machine-leases.json`, with lease locks in
`machine-lease-holds/`.

Keep the lease held while work runs. After it stops, call `machine_return(id)`
to free the slot. Return is idempotent. Dropping the handle releases the hold.
The reservation continues to occupy capacity until returned.
`machine_status()` reports `owner_ended` or `hold_released` when it has
evidence that a reservation has ended. It uses the same
[owner liveness rules](containers.md#lifecycle) as container leases.
An absent or mismatched registered volume for a missing checkout, or
uncertain owner or hold evidence, keeps the lease reserved for cleanup.

`barge machine hold <id> [--reason TEXT]` blocks new leases on a machine and
lets current leases finish. `barge machine release <id>` clears the operator
hold. `barge run status [--json]` shows the account's leases and holds.

## Local container leases

`Barge::run_resolve` takes a held worker lease for a container, with the
request's owner and a project name unique to the run. A full profile is
denied when the capacity wait expires, or immediately with no wait.

Keep the hold while the stack runs. Stop the stack before returning the
reservation with `container_return`. If the initiator disappears, the existing
[container lifecycle](containers.md#lifecycle) makes the stack eligible for
cleanup.

## Result record

A `RunResult` ties the outcome to the machine and actual platform, the
repository URL, base commit, optional patch digest and resulting tree ID.
It records the argument vector, optional toolchain, start and finish timestamps
in Unix milliseconds, and any wake time in milliseconds. It points to the log
with its digest and location, and can identify a Barge area and its byte size
after the run.

Initiators can construct the record directly with machine `"local"`. Outcomes
distinguish a process exit, a signal, an infrastructure failure with its stage,
cancellation, and budget expiry. The initiator interprets the command's output.

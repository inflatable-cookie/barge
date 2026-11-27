# Platform runs

An initiator supplies a platform constraint, minimum capabilities, source
identity, an argument vector, explicit environment entries, a wall-clock budget,
a fallback policy, and an owner. The owner identifies a worktree lease or an
absolute checkout path.

Barge resolves the request. The initiator executes local work and records the
result. The remote design assigns execution to barged in phase 3. The current
resolver tries the native host first, then a declared local container. A
remote-only request is denied because it forbids either local resolution.

Native matching uses the OS and architecture of the running Barge binary.
Minimum cores are compared with the parallelism available to the process.
Minimum memory is compared with total host memory. An unavailable measurement
cannot satisfy a minimum. GPU and positive VRAM requirements are unmet locally.
Container declarations cover platform and worker capacity. A container cannot
satisfy a positive core or memory minimum through these declarations.

## Local container targets

The operator puts `run-targets.json` in the account state directory beside
[`container-profiles.json`](containers.md#profiles).

```json
{
  "local_container": [
    {"profile": "apple-container", "platforms": ["linux-arm64", "linux-any"]}
  ]
}
```

Each `os-arch` entry can serve an exact, OS-only, or unrestricted request
that matches it. An `os-any` entry can serve an OS-only or unrestricted
request. `arm64` and `aarch64` are equivalent, as are `amd64` and `x86_64`.
Barge picks the first matching managed profile in file order. With no targets
file, only the native host can match.

`Barge::run_resolve` takes a held worker lease for a container, with the
request's owner and a project name unique to the run. If the profile is full,
the caller can supply a capacity wait and receive advisory positions among
waiting runs in the same process. Position starts at one and can change as
other waiters leave. All processes still compete for capacity through the
container lease API. A full profile is denied when the wait expires, or
immediately if the caller supplied no wait.

Keep the hold while the stack runs. Stop the stack before returning the
reservation with `container_return`. If the initiator disappears, the existing
[container lifecycle](containers.md#lifecycle) makes the stack eligible for
cleanup.

`barge run resolve --json < request.json` reads the request from stdin and
previews placement. A container preview names the profile and leaves capacity
untouched. See the [JSON contract](contracts/consumption.md#run-documents).

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

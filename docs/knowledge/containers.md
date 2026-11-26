# Container leases

Barge reserves one Compose project per runtime profile and records its owner.
The consumer starts and stops the stack. Effigy uses the lease API through a
pinned Barge library dependency.

## Profiles

The operator declares profiles in `container-profiles.json` in the account
directory, beside `layout.json`.

```json
{
  "profiles": [
    {"name": "effigy", "max": 4},
    {"name": "effigy-release", "max": 1}
  ]
}
```

Tom approved these initial limits on 2026-11-25. Each limit applies to its
profile alone. Interactive leases use no worker slots. The operator writes the
file once the consumer is ready. Missing and undeclared profiles return
`NotManaged`. A worker that reaches its capacity wait limit gets `Full`.

## Lifecycle

A lease is keyed by profile and project. The same live owner gets the
same ID and class on repeat requests. If it asks for a hold, the returned
`ContainerLease` keeps a shared flock until dropped. A lease without a hold
can be used for a detached stack. Barge keeps each reservation in
`container-leases/<id>.json` (`barge.container-lease/1`), with the hold alongside
it at `<id>.lock` when needed.

`container_status` lists every reservation. An owner ends when its worktree
lease is returned or gone, or its absolute checkout path disappears. That
produces `OwnerEnded` with `remove_volumes: true`. A worker whose last hold is
released while its owner is live shows `HoldReleased` with
`remove_volumes: false`, preserving data for the next run. Interactive leases
end only with their owner. If Barge cannot determine liveness, it leaves
`ended` empty, and a repeat request from the same owner keeps the lease ID.

Ended worker leases still count against the limit until returned. The leased
project stays reserved for cleanup. `container_return(id)` drops the
reservation and succeeds if the ID has already gone.

## Consumer duties

- Before leasing or during cleanup, read `container_status`. Tear down each
  ended stack, following `remove_volumes`, then return its lease on success.
  If teardown fails, keep the reservation so the next pass can retry.
- Label new stacks `barge.lease=<id>`. Clean up labelled stacks with IDs
  absent from status. The consumer coordinates teardown and startup for each
  project.
- Return the lease after a normal stop or a failed start.

## CLI

```
barge container status [--json]
barge container return <id> [--json]
```

Return releases the reservation after the stack has been stopped. See the
[JSON contract](contracts/consumption.md#success-documents).

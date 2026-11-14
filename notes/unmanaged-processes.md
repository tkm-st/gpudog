# GPU process observation and allocation gates

<!-- markdownlint-disable-next-line MD036 -->
_Last verified: 2026-11-02 against the unmanaged-process working tree_

## Summary

Real inventories always observe usage. No new reservation can use an observed
busy GPU, and observation failure/staleness pauses new grants. Held
reservations are never revoked and queue deadlines still run. Process-tree
labels are display metadata, not authorization or the busy predicate.

## Details

`ManagedNvmlSource` queries compute/graphics processes for managed UUIDs
only, validates UUID identity and reopens NVML after errors. The observer
uses one in-flight blocking worker outside the service state mutex. It pauses
allocation during IO, captures bounded host procfs facts and classifies
processes against held roots. Completed successful samples update rows,
the busy bitmap and freshness atomically, clear errors and pump waiters.
Stopping sets a persistent flag under the same mutex: late successful
worker results and holder releases cannot resume grants after `stop()`.
Failures preserve the last successful data and keep allocation paused.

`watch_unmanaged_ms` defaults to 1000 and accepts 1..=3600000. Source/procfs
failures log to stderr on error transitions; recovery logs and resumes.
There is no disable switch for real devices. Static `--num-gpus` inventories
explicitly bypass hardware observation for wire tests.

Peer PID/start-time identity is captured before the handshake completes.
`ProcessTracker` retains observed descendants keyed by `(pid, start_time)`
while their reservation is held. Cache entries are discarded for PID reuse
or released owners. Parent walks are cycle/depth bounded and procfs reads are
size bounded. Since 2026-11-12 the daemon does not scan /proc:
`ProcFs::collect` reads GPU pids and their ancestry, reservation roots,
known descendants and their children (`task/<tid>/children`), with a
descendant budget of `MAX_DESCENDANTS` shared per root. A whole scan was
both slow and an easy way to pause every grant (it failed past 100,000
entries). Missing identity/ancestry cannot prove ownership.
Unseen reparented processes can lose the association, but their usage still
blocks allocation. No process is signaled or requeued by the observer.

The daemon needs host PID/procfs visibility: Docker uses `--pid=host` without
user namespace separation or `--privileged`. Clients keep private PID
namespaces. Protocol v5 and later send daemon process rows, observation age/errors,
interval, paused state and a busy bitmap. Dash needs no local GPU/procfs
libraries, and `--no-gpu-processes` hides rows without changing daemon policy.
Busy GPU metadata is retained separately from truncated display rows; free
counts use the union of held and observed indices, never a sum that doubles
managed processes. Unavailable/stale samples show unknown free counts.

## Why

Allocation uses observed usage regardless of uncertain PID labels, so
reparenting/namespace uncertainty does not mark an occupied GPU usable.
Separating blocking work from scheduler state keeps peer cleanup and deadline
expiration responsive during observation failures. An external process can
still start after a sample; observation is not device access enforcement.

## Verification

Unit/service tests exercise busy/wrong/unknown usage, root/child PID reuse,
reparented descendants, failed initial/later observations, recovery,
preserved holders, queue expiry and invalid intervals. The CPU container
harness injects NVML usage/error files and verifies host child identities,
blocked grants, running-job survival and retry from independent client PID
namespaces. A native real-GPU probe created an external CUDA primary context,
verified its host PID/start-time and `not managed` label, timed out a queued
request without stopping the process, then observed release and granted
after context completion. This is driver/context verification, not training
or real MIG verification. Driver doubles are not hardware verification.

## See also

*   `docs/administration/unmanaged-processes.md`.
*   `notes/container-clients.md` and `notes/ipc-design.md`.

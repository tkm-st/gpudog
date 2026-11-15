# CUDA visibility and UUID resolution

`device_visibility::VisibleDevices` is the shared library boundary between
local CUDA ordinals and the daemon's UUID-based requests. Both binaries use
the same production discovery path.

## Discovery

`probe()` runtime-loads CUDA through cudarc's result API and calls `cuInit`,
`cuDeviceGetCount`, `cuDeviceGet` and `cuDeviceGetUuid`. It does not retain a
primary context, create streams, or allocate device memory. CUDA determines
the order and restrictions; NVML ordinals are never substituted.

For each visible UUID, NVML checks whether it is a MIG handle or has MIG
enabled. `NotSupported` means that GPU lacks the feature; other inspection
errors fail closed. The daemon fetches names/NVML indices only for visible
UUIDs, sorts that subset in NVML order and keeps all of it as fixed slots.
The `devices` allow-list only marks slots managed; daemon GPU numbers, the
shown slots in order, are recomputed only at startup and at a configuration
reload (see
[Changing the configuration](../administration/configuration.md#changing-the-configuration)).
Hidden NVML devices are not enumerated as fallback candidates.

CUDA caches its process-wide device view. Initialize visibility before using
CUDA elsewhere, and do not mutate environment variables afterward. A
snapshot's ordinals refer to that CUDA view, not to the daemon's inventory.
Docker needs `NVIDIA_DRIVER_CAPABILITIES=compute,utility`; no toolkit is
bundled. CUDA/NVML initialization errors are reported, never converted to
an unrestricted inventory. cudarc loader panics are contained as discovery
errors without replacing the application's panic hook.
Recovery assumes an unwind-enabled build and a non-aborting panic hook;
consumers built with `panic=abort` cannot recover from loader panics.

## Library use

`cli_options::build_acquire` parses options, but is not a complete wire
request: numeric lists and empty count candidates still need resolution.

```rust
use gpudog_client::cli_options::{build_acquire, RunOptions};
use gpudog_core::device_visibility::VisibleDevices;

let visible = VisibleDevices::probe()?;
let parsed = build_acquire(&RunOptions {
    gpus: Some("0,1".into()),
    ..RunOptions::default()
})?;
let wire_request = visible.prepare_acquire(&parsed)?;
```

`Client::acquire` obtains the real view on a blocking worker, prepares the
request and validates the grant. `acquire_with_visibility` accepts a trusted
provider snapshot, for alternative integrations and deterministic tests.
The CLI probes/prepares before connecting, then retains the same view through
acquisition. Successful count grants must stay in the resolved candidates;
explicit grants must also preserve the requested UUID order. A bad grant
shuts down the connection, causing EOF and reservation release. The same
cleanup applies to terminal read/codec errors and unexpected replies, not
just structurally valid grants that fail the view check.

After acquisition succeeds, `Client` allows only `ping` and
`into_reservation`. `status` and another acquisition return a local error
without releasing the GPUs. Open a separate connection to request status.
`into_reservation` requires a successful acquisition.

Dropping a pending acquisition future closes the connection once request
preparation succeeds, even if the caller retains `Client`.
The cancelled client cannot be reused. Local validation errors before this
point leave the connection usable.

Only the daemon enforces `Acquire::timeout_ms`, from when it receives the
request; the client has no deadline timer of its own. Expiry arrives as the
daemon's `Denied` with `DenyReason::TIMED_OUT`, so the daemon log records the
denial. Until the first reply, `REPLY_TIMEOUT` bounds the wait; after
`Queued`, a daemon that stops answering is waited for indefinitely.

Protocol v7 retains v4's rejection of numeric selection and empty candidates. Display
indices remain in grants/status. This is coordination for cooperating
clients, not proof of another process's visibility or GPU access control.

## Verification

Unit tests use injected snapshots for ordinal order, missing UUIDs,
duplicates, bounds, candidate intersection and invalid grants. Real binary
tests use isolated CUDA/NVML driver doubles to check numeric/UUID/empty
visibility, MIG failure and daemon startup restrictions without GPUs.
Current golden frames come from the independent Python v7 generator.

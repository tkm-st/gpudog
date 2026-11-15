# GPUDog decision record

<!-- markdownlint-disable-next-line MD036 -->
_Last verified: 2026-11-02 against the unmanaged-process working tree_

## Summary

Current implementation details are in `crates/`. This note records the
decisions that still hold, not future requirements.

## Transport and identity

*   A `SOCK_STREAM` Unix socket provides kernel credentials, deferred grants
    and release on connection closure without a network surface.
*   The socket directory is shared between the daemon container and host.
    Its ownership and permissions prevent socket replacement attacks.
*   `SO_PEERCRED` identifies users; unmapped/overflow UIDs are rejected.
    Clients run on the daemon's host, directly or in containers that share
    the host user namespace and the socket directory.
*   Connection and per-user caps bound the fd-based reservations. Pre-acquire
    reads and writes have deadlines.

See `notes/ipc-design.md` for the rejected alternatives.

## Reservations and scheduling

*   The socket descriptor is inherited by the child. Killing `gpudog` alone
    does not release a GPU that the child still uses.
*   After spawning, `gpudog run` outlives SIGINT, SIGQUIT and SIGHUP (the
    terminal already delivers them to the command), forwards SIGTERM to the
    command only, and returns the command's status. A command killed by
    SIGINT makes `gpudog` re-raise SIGINT through signal-hook, because bash
    only stops a script loop on Ctrl-C for a job that died of SIGINT
    (2026-11-09).
*   Catching a signal replaces an inherited SIG_IGN, and exec then resets it
    to the default, so `nohup gpudog run` would expose the command to SIGHUP.
    `gpudog` therefore never catches a signal listed in `SigIgn:` of
    `/proc/self/status`. Tests that need sh to trap SIGINT skip when the test
    runner itself started with SIGINT ignored.
*   Killing `gpudog` between the grant and the spawn correctly releases the
    GPUs (no child holds the descriptor yet). Tests that kill `gpudog` must
    wait until the command has started, not just until the grant.
*   Whole GPUs are reserved exclusively among GPUDog requests. The queue is
    walked by user load (held GPUs, free GPUs busy with the user's own
    processes, and its earlier waiting requests), then arrival. A request
    that cannot be granted reserves only the slots it could use, and only
    its user's own slots when it cannot be met without them, or only the
    slots busy outside GPUDog when it cannot be met without those, so no
    user can choose to stall the queue indefinitely (security review S1,
    2026-11-12). What is left by design: a request waiting for another
    user's reservation keeps the free slots it needs until that ends.
    Per-user rules count per uid, and any uid may connect (developer
    decision, 2026-11-12), so subordinate uids act as separate users.
    Clients send no priority since protocol v6: any user could pick the
    most favoured one.
*   Impossible requests are rejected immediately; timeouts and user caps
    apply to queued requests too. Release is idempotent.
*   The clock is supplied to the scheduler for deterministic tests.
*   tokio `select!`, `oneshot` and `SessionGuard::drop` handle queued peer
    death, grants delivered before waiting, and runtime cancellation.
    The service never holds its state mutex across `.await`.

## Client and GPU identity

*   Requests are parsed before connecting. `-n` and `-g` are exclusive;
    `-g` accepts either indices or UUIDs, not a mixed list. `-t 0` is refused
    because zero on the wire means wait forever.
*   Requested GPU indices are resolved in the client's CUDA environment.
    Protocol v7 retains UUIDs and requires non-empty count candidates. Numeric,
    empty and container visibility restrictions are not discarded. Dense
    daemon numbers are internal/display metadata; children receive UUIDs.
*   The daemon never changes OS nice values. Only SIGTERM is
    forwarded; terminal signals already reach the foreground group.

See `notes/device-identity.md` for the exact current behaviour.

## Configuration and logs

*   Defaults, TOML config, then explicit command-line overrides are merged
    in that order and validated after merging.
*   Unknown or wrongly typed keys warn; malformed TOML or impossible values
    stop startup. Invalid `devices` is fatal, never a fallback to all GPUs.
*   The config file is opened once, checked on its descriptor, and bounded
    to 1 MiB. FIFOs and unsafe ownership/modes are refused.
*   Logging uses stderr/journal through a bounded writer queue. The legacy
    log-directory key/option warn and are ignored.

## Dashboard and observed processes

*   `dash` is terminal-only and opens a fresh connection per refresh.
*   Command text is untrusted display data, sanitized at the boundary and
    in renderers. Status snapshots truncate with a warning rather than fail.
*   The daemon queries NVML and walks `/proc` (`gpu_processes`,
    `process_watcher`); `dash` only renders the daemon's snapshot.
    Classification is best-effort, not authorization or scheduling input.
    Double-fork outside a known parent, unreadable procfs and the
    descendant and ancestor budgets (`MAX_DESCENDANTS`, `MAX_ANCESTORS`)
    limit it.

## Why

These decisions are recorded so that a simpler-looking change does not
reopen them.

## See also

*   `notes/design-notes.md`: implementation choices and pitfalls.
*   `docs/developer/architecture.md`: current module layout.

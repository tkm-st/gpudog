# Security model

`gpudogd` runs as root and accepts requests from unprivileged local users.
This page explains what it trusts and why some of its checks cannot be
turned off safely.

## Identity

The daemon learns each client's uid and pid from the kernel (`SO_PEERCRED`)
when the client connects. The client never sends its own identity, so it
cannot pretend to be someone else.

If the uid cannot be resolved, for example because the container uses user
namespace remapping and the host uid has no mapping, the daemon rejects the
connection. Otherwise every unmapped user would share one identity and one set
of per-user limits.

## No network exposure

The daemon only listens on a Unix domain socket. It opens no TCP port and
needs neither `--network host` nor any authentication token.

## Socket directory

The socket directory (`/run/gpudog` by default) must be owned by root, or by
the daemon's user, and must not be writable by group or others. If another
user could write to it, they could delete the socket and put their own in its
place. Every client would then talk to them instead of the daemon and could be
told it holds GPUs that are in use.

When the directory is missing, the daemon creates it, owned by its own user
with mode `0755`, and then checks it like any other. It creates only that
last directory, never its parents, and does not change the mode of anything
it did not just create.

`require_secure_directory = false` and `--allow-insecure-socket-directory`
turn this check off. Use them only for local testing.

## Configuration and logging

The configuration file must be a regular file owned by root or the
daemon's uid, not writable by group or others. Write access to it is control
over the daemon. See [Configuration](configuration.md).
The daemon logs to stderr/journal and no longer opens application log files
as root. See [Logging](logging.md).

## Untrusted input

*   Every message size, list length and string length is checked before any
    memory is allocated. A frame is at most 64 KiB.
*   A client has `handshake_timeout_ms` for each message before it is
    granted or queued. A client that connects and goes silent is
    disconnected.
*   Connection counts, GPUs held and queued requests are limited per user.
*   No user can choose to hold up the queue for others indefinitely.
    Clients cannot choose a priority; the daemon serves users who use fewer
    GPUs first. A waiting request holds up only requests that need the GPUs
    it is waiting for. A request that waits for its own user's GPUs, or for
    a GPU in use outside GPUDog, holds up only those GPUs (see
    [The queue](../concepts.md#the-queue)).
*   What remains by design: a user who uses nothing can ask for many GPUs,
    up to `max_held_per_uid`, while one of them is held by another user's
    job. The free ones are then kept for that request until the job ends,
    however long that is. Set `max_held_per_uid` below the number of GPUs
    if one user must not be able to keep a whole machine idle.
*   Also by design, anyone can start a process outside GPUDog on one of the
    GPUs a large waiting request needs. That request then keeps only the
    busy GPU and any GPU its own user is using, and smaller requests,
    including the starter's own, can take the others first. The starter's
    GPU then counts towards how many GPUs the starter already uses, and the
    process shows in `gpudog dash` as `not managed`.
*   These rules count per uid, and any uid may connect, including uids
    created inside containers. So one person with a range of subordinate
    uids (`/etc/subuid`, as rootless containers use) can connect as several
    users, each with its own per-user limits and its own place in the
    queue, and can get around the rule that waiting for your own GPUs holds
    up nobody else. GPUDog does not prevent this.
*   A Docker daemon must see the host's processes (`--pid=host`) to know
    whose process uses a GPU; without it, it logs
    `no observed GPU process has a readable owner` once.
*   The command line a client reports is used only for display. It may be at
    most 256 bytes; a longer one is a protocol violation and the connection
    is closed. It is reduced to printable ASCII before it is logged, and
    escaped again by the dashboard.

## Information visible to all users

Any user allowed to connect to the socket can see reservations and
daemon-observed GPU processes: host PID, UID, process name, start-time ticks,
memory usage and ownership labels, plus reservation command lines. This
centralizes host metadata even when a client has a private PID namespace
or a restricted procfs view. Do not put secrets in reported command text;
restrict socket permissions if this information must not be public.

## Directories the daemon trusts

The daemon checks the configuration file (see
[Configuration and logging](#configuration-and-logging)) and, unless that
check is turned off, the socket's own directory (see
[Socket directory](#socket-directory)). It does not check the
directories that contain them, or any directory above. Anyone who can
write to one of those directories, such as
`/etc/gpudog`, `/etc`, `/run` or the parent of a custom `--config` or
`socket_path`, can:

*   remove or rename the configuration file, or a directory above it, so
    that the daemon starts with the default settings the next time (it
    logs `no configuration file at <path>; using the defaults`), or, with a
    custom `--config`, does not start;
*   put a symlink in its place that points to another file owned by root.
    The daemon checks the file the symlink points to, so it passes. A file
    that is not TOML stops startup and is refused at a reload, but an empty
    one is accepted, at a reload as well: the daemon then manages every GPU
    and the per-user limits return to their defaults;
*   replace the socket directory, so that clients reach a socket they
    control instead of the daemon's.

Keep every directory on both paths owned by root and not writable by
others. The Debian package and the default paths (`/etc/gpudog`,
`/run/gpudog`) already do.

## Host process observation

The daemon container requires `--pid=host` for process-tree classification.
This increases its host process visibility and the impact of any future
process-signaling bug. The observer does not signal, stop, requeue or change
process priority, and does not require `--privileged` or a Docker socket mount.
The client keeps its own PID namespace. The observer reads only GPU
processes and their parent chains, each job's `gpudog run` process and its
descendants, and processes it already knows to belong to a job, not all of
`/proc`. PID/start-time identities help avoid reusing
old labels; labels are not process-operation permissions.

Observed usage blocks new allocation regardless of ownership labels.
Observation failures/staleness pause new grants without revoking existing
jobs, but cannot prevent an external process starting after a sample. See
[Unmanaged processes](unmanaged-processes.md).

## Limits of the model

GPUDog prevents accidents, not abuse. A user can ignore
`CUDA_VISIBLE_DEVICES` or skip `gpudog` entirely. See
[Concepts](../concepts.md#what-gpudog-does-not-do).

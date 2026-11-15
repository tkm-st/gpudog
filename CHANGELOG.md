# Changelog

Notable changes to GPUDog. Each release also has a Debian changelog entry in
`packaging/debian/changelog`.

## Compatibility

Releases before 1.0 make **no compatibility promise**. Any release may change
the wire protocol, the configuration file, command-line options, messages or
log lines. Every such change is listed in
[CHANGELOG.md](https://github.com/tkm-st/gpudog/blob/main/CHANGELOG.md)
under the release that makes it, and each release's notes include that
release's section.

*   Upgrade `gpudog` and `gpudogd` together, including client container
    images. A client and a daemon that speak different protocol versions
    refuse each other with `protocol version mismatch`: running jobs keep
    their GPUs, but new
    `gpudog` commands fail until both sides match.
*   Read the changelog before upgrading. Restarting the daemon ends every
    reservation and fails waiting requests; running commands keep running,
    but GPUs they are not using yet may then be given to others. See
    [Upgrading the package](https://github.com/tkm-st/gpudog/blob/main/docs/installation.md#upgrading-the-package)
    for when to restart.

## [0.0.1] - Unreleased

First release.

### Added

*   `gpudogd`, a daemon that hands out exclusive GPU reservations to local
    users over a Unix socket, identifies users through the kernel, and keeps
    a queue with per-user limits on connections, held GPUs and waiting
    requests. The daemon orders the queue itself: users who use fewer GPUs
    are served first, and a waiting request holds up only requests that
    need the GPUs it is waiting for. There is no client-chosen priority.
*   `gpudog run`, which reserves GPUs by count (`-n`) or by CUDA number or
    UUID (`-g`), with a queue timeout (`-t`). It sets
    `CUDA_VISIBLE_DEVICES` to the reserved UUIDs and holds the reservation
    while the command, or anything it starts that keeps the connection
    open, runs.
*   `gpudog health`, which checks that the daemon answers, and
    `gpudog dash`, a terminal view of reservations, waiting requests and
    observed GPU processes.
*   Daemon logs on standard error, which systemd sends to the journal. A
    warning or error that a client can cause repeatedly, such as going over its
    connection limit, is logged at most once every 10 seconds per user. A
    missing default configuration file is logged as a warning.
*   A missing socket directory is created at startup (mode `0755`, the
    last directory only) and logged as `created socket directory <path>`.
*   GPU process observation: a GPU observed in use is not granted until it
    is idle, whoever started the process, and failed observations pause new
    grants without stopping running jobs. The daemon reads from
    `/proc` only the processes it needs to observe GPU usage, and logs when
    a job has too many processes to follow or the kernel does not list
    child processes.
*   `/etc/gpudog/config.toml`. `SIGHUP` (`systemctl reload gpudogd`)
    reloads every setting except the three socket settings.
*   A Debian package with a systemd unit and a commented configuration
    template, a release tarball for x86-64 Linux with glibc 2.28 or later,
    and a Dockerfile for the daemon and for client images. Container images
    are not published.
*   Wire protocol version 6. Earlier versions were used only by development
    builds.

### Known limitations

*   GPUDog coordinates users who agree to cooperate; it is not access
    control. A process can ignore `CUDA_VISIBLE_DEVICES`, and processes
    started without `gpudog run` are observed but not stopped.
*   The daemon measures `gpudog run -t`. If the daemon stops responding
    after a request is queued, `gpudog run` keeps waiting until it is
    interrupted.
*   GPUs are reserved whole. MIG instances and GPUs with MIG enabled are
    refused.
*   Clients must run on the same host as the daemon. Separate-container
    clients need host user and group IDs (no user namespace remapping).
*   The daemon runs as root, and by default every local user can connect to
    its socket; it tells users apart by their kernel user ID. See
    [Socket permissions](https://github.com/tkm-st/gpudog/blob/main/docs/administration/configuration.md#socket-permissions).
*   Per-user limits and the queue order count each uid separately, and any
    uid may connect. One person with subordinate uids, as rootless
    containers use, can act as several users. See
    [Untrusted input](https://github.com/tkm-st/gpudog/blob/main/docs/administration/security.md#untrusted-input).
*   Restarting the daemon ends every reservation, and the new daemon can
    hand a reserved GPU that is not in use yet to someone else. A package
    upgrade therefore does not restart a running daemon.
*   A GPU added or made visible after the daemon started is used only after
    a restart.
*   Release artifacts are built for x86-64 Linux only. CI and the release
    workflow run on machines without GPUs, with test doubles for the NVIDIA
    driver; neither checks behaviour on real GPUs.
*   The release artifacts are checksummed but not signed. There are no man
    pages; use `--help` and the documentation.
*   The licenses of the Rust crates built into the binaries are in
    `THIRD-PARTY-LICENSES`, in the release tarball and in the release
    Debian package. Debian packages built from source with `cargo deb` and
    images built from the Dockerfile do not include it.

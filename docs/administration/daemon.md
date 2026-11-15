# Running the daemon

`gpudogd` manages its configured CUDA-visible GPUs and listens on a Unix
domain socket. Run one daemon per GPU pool; pools must not overlap.

## With Docker (recommended)

```shell-session
$ docker run -d --name gpudogd --restart unless-stopped --pid=host --userns=host --gpus all \
    -v /run/gpudog:/run/gpudog \
    -v /etc/gpudog:/etc/gpudog:ro \
    gpudogd
```

What each flag is for:

| Flag | Why |
| -- | -- |
| `--gpus all` | The daemon must see every GPU it manages. A subset also works; see below. |
| `--pid=host` | Gives the observer host PID/procfs visibility for process-tree identities. Clients keep their own PID namespaces. |
| `--userns=host` | Keeps host UIDs on an engine with user namespace remapping enabled; without it, the daemon rejects clients whose UIDs have no mapping. It has no effect on an engine without remapping. |
| `-v /run/gpudog:/run/gpudog` | Shares the socket with clients on the host. A rootful Docker engine creates the host directory, owned by root, if it is missing. If yours does not (for example with user namespace remapping enabled), add `d /run/gpudog 0755 root root -` in `/etc/tmpfiles.d/gpudog.conf`, which recreates it at every boot. |
| `-v /etc/gpudog:/etc/gpudog:ro` | Optional. Supplies `config.toml`; mount it read-only. The image has no `config.toml`, so without this mount the daemon uses the defaults and logs a warning at every start. |

Do **not** use these flags:

*   `--network host`: the daemon does not listen on any network port.
*   User namespace remapping: users without a mapping all show up as the
    same overflow uid. The daemon rejects those connections, so remapping
    breaks GPUDog completely.

For separate-container clients, explicitly use `--userns=host` on both
containers and pass the invoking host UID/GID to the client. Share the same
socket directory; see [Container clients](../user-guide/container-clients.md).

To manage only some GPUs, either give the container a subset, for example
`--gpus '"device=1,2"'`, or keep `--gpus all` and list the GPUs under
`devices` in [`config.toml`](configuration.md). Grants name GPUs by UUID, so
clients on the host get the right devices either way. Only one daemon may
manage a given GPU.

The image's default command is
`--socket /run/gpudog/gpudogd.sock`. Arguments
after the image name replace it.

## Without Docker

Run the binary as root, for example from a systemd unit:

```shell-session
$ sudo gpudogd --socket /run/gpudog/gpudogd.sock
```

The Debian package ships `gpudogd.service`, which creates `/run/gpudog`
and starts the daemon. Read its logs with `journalctl -u gpudogd`. The unit
sets `ProtectSystem=strict`: apart from `/run/gpudog` (and `/dev`, `/proc`
and `/sys`), the file system is read-only for the daemon, so it creates a
missing socket directory only in `/run/gpudog`. To put the socket in another
directory, for example `/srv/gpudog`:

1.  Create the directory, owned by root with mode `0755`:
    `sudo install -d -m 0755 /srv/gpudog`. systemd refuses to start the unit
    (status `226/NAMESPACE`) when the directory does not exist.
2.  Set `socket_path = "/srv/gpudog/gpudogd.sock"` in `config.toml`.
3.  Make only that directory writable, never a shared parent such as `/srv`
    or `/tmp`, with `sudo systemctl edit gpudogd`:

    ```ini
    [Service]
    ReadWritePaths=/srv/gpudog
    ```

4.  Run `sudo systemctl restart gpudogd`.

Clients then need the same path, for example
`gpudog run -s /srv/gpudog/gpudogd.sock -- ...` (`health` and `dash` take
`-s` too).

## Command-line options

| Option | Description | Default |
| -- | -- | -- |
| `-c`, `--config` | TOML configuration file. | `/etc/gpudog/config.toml` |
| `-s`, `--socket` | Unix domain socket to listen on. | `/run/gpudog/gpudogd.sock` |
| `-l`, `--log-directory` | Obsolete option, not listed by `--help`; warns and is ignored. Logs go to stderr/journal. | unused |
| `--max-connections` | Maximum number of client connections at once, `1` to `1000000`. | `256` |
| `--handshake-timeout-ms` | Milliseconds a client may take for each message before it asks for GPUs, `1` to `3600000`. | `10000` |
| `-n`, `--num-gpus` | Skip CUDA/NVML/MIG discovery and fabricate this many GPUs, `0` to `1024`. For wire/service tests only. | enumerate real visible GPUs |
| `--allow-insecure-socket-directory` | Start even if the socket directory is writable by others. **Unsafe.** | off |
| `-V`, `--version` | Print the version and exit. | |

Options given on the command line override the configuration file. See
[Configuration](configuration.md).

## Startup

At startup the daemon:

1.  Checks the configuration file. If it is not a regular file, is owned by
    someone other than root or the daemon's user, or is writable by group or
    others, the daemon refuses to start.
2.  Loads the configuration and applies command-line overrides. An invalid
    value stops startup. A file that does not parse is reported on one
    line with its position, for example
    ``/etc/gpudog/config.toml: invalid string; expected `"`, `'` (line 4, column 19)``
    (prefixed with `gpudogd: error:` outside the journal).
3.  Resolves its CUDA-visible UUIDs, rejects MIG, reads NVML metadata only for
    that view, and applies the config allow-list. Discovery or validation
    failure stops startup; GPUs outside that view are never used. The
    daemon handles at most 1,024 GPUs; with more visible, it refuses to
    start with
    `cannot enumerate GPUs: CUDA device count exceeds the supported range`
    (prefixed with `gpudogd: error:` outside the journal).
    Narrow its view with `CUDA_VISIBLE_DEVICES` (or Docker `--gpus`).
4.  Creates the socket directory with mode `0755` if it is missing, and logs
    `created socket directory <path>`. Under a parent that fails the same check
    (for example one others can write to), it keeps the mode the umask gives instead and warns
    `left mode <mode> from the umask on <path>: ...`; see
    [Security model](security.md#socket-directory). Only the socket directory itself is
    created: if its parent is missing too, startup fails with
    `cannot create <path>: No such file or directory` (prefixed with
    `gpudogd: error: startup failed:` outside the journal), where `<path>`
    is the socket directory. Then it checks the
    socket directory, binds the socket, and sets its permissions to
    `socket_mode`.

Startup, requests, warnings and errors are logged to stderr through a
bounded writer queue. The first line, logged before the checks above, names
the version, for example `gpudogd 0.0.1 (protocol v6) starting`. See
[Logging](logging.md).
New grants wait for successful GPU process observation and remain paused
on query failures/staleness. Queue timeouts still run. See
[Unmanaged processes](unmanaged-processes.md).

## Reloading the configuration

`SIGHUP` (`systemctl reload gpudogd`, or `docker kill -s HUP gpudogd`) reloads
`config.toml` without stopping the daemon. Running jobs are not affected, even
on a GPU that the reload removes from `devices`, and waiting requests keep
waiting unless the new per-user GPU cap or `devices` makes them impossible. A `SIGHUP` that arrives in the first moments after the process
starts, before it installs its handler, still ends the daemon. The packaged
service starts it again within about 10 seconds; elsewhere, do not reload a daemon
that is still starting: wait until the log shows `listening on ...`. See
[Changing the configuration](configuration.md#changing-the-configuration) for
what a reload changes.

## Shutdown

`SIGINT` or `SIGTERM` (for example `docker stop`) stops the daemon cleanly and
removes the socket file. Their connections are closed, so waiting clients exit
with `gpudog: lost the connection to gpudogd: <reason>`.

Stopping the daemon does not stop processes that are already running, but
their reservations are lost. A newly started daemon grants nothing until its
first GPU process observation succeeds, and it does not grant a GPU on which
it observes a running process. A job that has its GPUs reserved but has not
started using them yet (for example while it loads data) is invisible, so the
new daemon may give the same GPUs to someone else. Restart the daemon only
when the GPUs are idle, or when every running job is already using its GPUs.
Automatic restarts after a crash (`Restart=on-failure` in the packaged unit,
`--restart unless-stopped` for Docker) carry the same risk. Upgrading the
Debian package does not restart the daemon for this reason; see
[Upgrading the package](../installation.md#upgrading-the-package).

## Failures while running

### A job holds GPUs too long

The daemon never takes GPUs back from a running job, even one that hangs.
`gpudog dash` shows the uid and the pid of the `gpudog` process for each
reservation. For a daemon in a container, read the pid as described in
[Dashboard notes](../user-guide/dashboard.md#notes). Killing only that
`gpudog` process does not release the GPUs; the command keeps the reservation.
Find the command and its descendants with `ps -ef --forest` and stop them, for
example with `kill <pid>`. The GPUs are released when the last of them exits.

### New grants are paused

If GPU process observation fails, the daemon pauses new grants on **every**
GPU and logs `GPU observation failed; new grants paused: <reason>`. A failure
on one GPU, for example one that fell off the bus, is enough. Running jobs
continue and queue timeouts still apply. Grants resume when an observation
succeeds again, which is logged as
`GPU observation recovered; new grants resumed`.

If a GPU stays broken, remove it from `devices` in
[`config.toml`](configuration.md) and reload the daemon. The daemon stops
observing that GPU at once, even while a job still holds it, so grants on the
other GPUs resume after the next successful observation. If observation still
fails, or the NVIDIA driver was reloaded, restart the daemon: it finds the
GPUs only when it starts (see [Shutdown](#shutdown) for what a restart means
for running jobs).

### Clients cannot connect although the daemon runs

The daemon does not notice if its socket file or `/run/gpudog` is deleted
while it runs, for example by a cleanup job. Clients then fail with
`gpudog: cannot reach gpudogd at <path>: No such file or directory`.
Restart the daemon to recreate the socket (see [Shutdown](#shutdown) for
what a restart means for running jobs).

### Startup at boot

The packaged service starts after `nvidia-persistenced.service` when that
service exists. If the NVIDIA driver is not ready yet, startup fails and
systemd retries every 10 seconds without giving up. A configuration error is
retried the same way; `journalctl -u gpudogd` shows the reason. To stop the
retries while you fix the file, run `systemctl stop gpudogd`, then start it
again. Until the daemon runs, clients fail with
`gpudog: cannot reach gpudogd at <path>: No such file or directory`.

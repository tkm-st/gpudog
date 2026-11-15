# Troubleshooting

## `cannot reach gpudogd at ...: No such file or directory`

```text
gpudog: cannot reach gpudogd at /run/gpudog/gpudogd.sock: No such file or directory
gpudog: is gpudogd running?
```

The socket does not exist. Check that:

*   The daemon is running: `docker ps` and `docker logs gpudogd` for Docker,
    or `systemctl status gpudogd` and `journalctl -u gpudogd` for the packaged
    service.
*   The socket was not deleted while the daemon kept running, for example by a
    cleanup job; see
    [Clients cannot connect although the daemon runs](administration/daemon.md#clients-cannot-connect-although-the-daemon-runs).
*   `/run/gpudog` is bind-mounted into the container. Without the mount, the
    daemon creates the socket inside the container where the host cannot see
    it.
*   The client and the daemon use the same socket path (`-s`/`socket_path`).

## `... Connection refused`

A socket file exists, but nothing is listening on it. The daemon probably
crashed or was killed without cleaning up. Start it again; the packaged
service restarts it by itself within about 10 seconds. The new daemon replaces
the stale socket file.

## `... Permission denied`

The socket mode does not allow you to connect. Check `socket_mode` in the
[configuration](administration/configuration.md) and the permissions of
`/run/gpudog`. If the daemon logged `left mode ... from the umask on ...`, it created
the socket directory under an unsafe parent with a restrictive umask:
`sudo chmod 0755` the directory, or put the socket under `/run/gpudog`.

## `... gpudogd did not answer within 10 seconds`

```text
gpudog: cannot reach gpudogd at /run/gpudog/gpudogd.sock: gpudogd did not answer within 10 seconds
```

Something accepted the connection but did not answer. Every `gpudog`
command gives up after 10 seconds instead of hanging. Check that:

*   The socket path (`-s`/`socket_path`) is right and belongs to `gpudogd`.
*   The daemon is not stuck or overloaded: ask the administrator to check
    `journalctl -u gpudogd` (or `docker logs gpudogd`) and restart it when
    the GPUs are idle.

This limit applies only until the daemon first answers. Once your request is
in the queue, `gpudog run` waits as long as `--timeout` allows; see
[`gpudog run` keeps waiting after its timeout](#gpudog-run-keeps-waiting-after-its-timeout).

## The daemon refuses to start

| Message | Fix |
| -- | -- |
| `... is writable by group or others, which would allow a local user to replace the socket` | `sudo chmod 0755 /run/gpudog` |
| `... is owned by uid N, which is neither root nor this process` | `sudo chown root:root` the directory. |
| `cannot create ...: No such file or directory` | The daemon creates the socket directory but not its parents. Create the parent, or use the default socket path under `/run/gpudog`. |
| `another gpudogd is already listening on ...` | Only one daemon may run per socket. |
| `... exists and is not a socket` | Remove the file at the socket path. |
| Configuration ownership or mode errors | Make `config.toml` a regular file owned by root or the daemon's uid with mode `0644` or stricter. |
| `no such configuration file: ...` | The file given with `--config` does not exist. |
| `no configuration file at /etc/gpudog/config.toml; using the defaults` (warning) | Expected when you use the defaults. To silence it, create the file (an empty file is valid), or in Docker mount `/etc/gpudog` with `-v /etc/gpudog:/etc/gpudog:ro`. |
| `max_connections must be at least 1` and similar | Fix the value in `config.toml`. See [Configuration](administration/configuration.md). |
| `cannot enumerate GPUs: ...` | The NVIDIA driver (CUDA or NVML) is not available. Check `--gpus all` and the NVIDIA Container Toolkit; see [CUDA driver or MIG inspection errors](#cuda-driver-or-mig-inspection-errors). |

## `configuration reload failed; keeping the previous configuration: ...`

The daemon got `SIGHUP` (for example from `systemctl reload gpudogd`) but did
not apply the file, and kept every setting as it was. The reason follows the
colon. A reload refuses a missing file, and values of the wrong type or out of
range, even though startup accepts them (at startup a missing default file
means the defaults). It also refuses a `devices` entry for a GPU the daemon
did not see when it started: restart the daemon, when the GPUs are idle, to
use that GPU (see [Shutdown](administration/daemon.md#shutdown)). If a value
was refused, the `ignoring '<key>': ...` warning just before this line names
the setting. Fix the file and reload again. See
[Changing the configuration](administration/configuration.md#changing-the-configuration).

## Request denied

`gpudog run` prints a summary, then the daemon's reason, and exits with
status `1` without starting the command:

```text
gpudog: request can never be satisfied: requested 4 GPUs but this host has only 2 GPUs
```

| Message | Meaning |
| -- | -- |
| `request can never be satisfied: requested N GPUs but this host has only M GPUs` | Ask for fewer GPUs. For a waiting request, the administrator may have removed GPUs from the daemon. |
| `CUDA number N is not visible (M visible in this CUDA environment)` | `N` is a [CUDA number](concepts.md#gpu-numbers). Check your `CUDA_VISIBLE_DEVICES` and container GPUs, not the daemon GPU numbers in `dash`. |
| `invalid request: requested N GPUs but the per-user cap is M GPUs` | The request alone asks for more than `max_held_per_uid`. A waiting request also gets this when an administrator lowers the cap below what it asks for. Ask for at most `M` GPUs. |
| `invalid request: this user already holds N GPUs; the per-user cap is M GPUs` | When you asked, the GPUs you already held plus this request were more than `max_held_per_uid`, so the request was refused, not queued. Run it again when your other jobs have ended, or ask for fewer GPUs. A request that was already waiting is not refused this way; it keeps waiting (see [The queue](concepts.md#the-queue)). |
| `invalid request: this user already has N requests waiting; the per-user cap is M requests` | You already have `max_queued_per_uid` requests waiting. |
| `invalid request: too many concurrent connections for this user` | You have too many `gpudog` processes open. |
| `the daemon reported an internal error: connection limit reached` | The daemon has reached `max_connections`. |
| `timed out waiting for GPUs: no GPU became available before the deadline` | The `-t` timeout ran out. |

An older daemon counts in "devices" in these messages, for example
`this user already has N devices waiting`; the meaning is the same, and the
waiting count is in requests. Older versions also print `device GPU-...` for
the UUID messages below.

## Other client errors

| Message | Meaning |
| -- | -- |
| `the following required arguments were not provided` | Nothing follows the `run` options, so the command is missing. |
| `cannot run ...: No such file or directory` | The command was not found (exit status `127`). The GPUs are released. |
| `the command was killed by signal N (SIGNAME)` | Your command was killed by that signal, for example `SIGKILL` from the out-of-memory killer. The exit status is 128 + *N*; see [Exit status](user-guide/run.md#exit-status). |
| `--gpus ...`, `--timeout ...`, `--num_gpus ...` | An option value is invalid; the message explains the expected form. |
| `lost the connection to gpudogd: ...` | The daemon stopped while you were waiting. A command that is already running is not affected and prints nothing. |

## `protocol version mismatch`

```text
gpudog: protocol version mismatch: daemon speaks protocol version 6, client speaks 5
```

The client and the daemon were built from incompatible versions. Install
matching versions of `gpudog` and `gpudogd`. Right after a package upgrade,
the matching daemon is already installed but the previous one still runs:
ask your administrator to restart it (see
[Upgrading the package](installation.md#upgrading-the-package)).

## `--gpus does not support MIG devices yet`

`-g` accepts CUDA numbers or `GPU-<uuid>` strings, but not MIG instances.

## `GPU-... is not managed by this daemon`

The UUID given with `-g` is not one of the daemon's GPUs. `gpudog dash` lists
the GPUs it manages with their UUIDs, and for a while GPUs just removed from
it, which cannot be requested. A waiting request gets this message too,
as `request can never be satisfied: ...`, when an administrator removes the GPU
from the daemon.

## `GPU-... is not visible in this CUDA environment`

The UUID is not visible in your environment. Check `CUDA_VISIBLE_DEVICES`
(numbers or UUIDs) and which GPUs your container can access. The command
does not run and the client does not request this GPU from the daemon.

## `requested N GPUs but only M visible in this CUDA environment`

With `-n`, fewer GPUs than you asked for are visible to you. Ask for fewer
GPUs, or make more GPUs visible.

## `requested N GPUs but only M GPUs visible to the client and managed by gpudogd`

With `-n`, fewer GPUs than you asked for are both visible to you and managed
by the daemon. Ask for fewer GPUs, or make more of the daemon's GPUs
visible. An older daemon reports this as `... the CUDA candidate set
includes only M devices managed here`.

## `daemon GPU N does not exist; this host has M GPUs`

Requests name GPUs by CUDA number or UUID, never by daemon GPU number, so
no current client should get this message. If you see it, report it to
your administrator with the command you ran.

## `no GPUs are visible in this CUDA environment`

No GPU is visible in your environment, for example because
`CUDA_VISIBLE_DEVICES` is empty, `-1`, or starts with an invalid entry. GPU
requests fail instead of replacing the restriction. If some GPUs are visible
but too few, the client reports the requested and visible counts before
requesting anything.

## CUDA driver or MIG inspection errors

`CUDA driver unavailable or missing required symbols` means `libcuda.so.1`
cannot be loaded or lacks required functions. A missing NVML library is
reported as `libnvidia-ml.so.1 not found; ...` instead. Both `gpudog run`
and the daemon need the NVIDIA driver. Inside Docker, expose both `compute`
and `utility` driver capabilities. If `nvidia-smi` works but the message
says symbols are missing, the driver is probably too old: update it, or on
a shared host ask your administrator.
MIG instances and MIG-enabled visible GPUs are unsupported; an inability
to inspect MIG state is an error, not a fallback to unchecked GPUs.

## `failed to discover GPU vendor from CDI`

```text
docker: Error response from daemon: failed to discover GPU vendor from CDI: no known GPU vendor found
```

Docker could not give the container any GPU, so the program in the
container (`gpudogd` or `gpudog`) did not start. `--gpus` on `docker run`,
and `gpus:` in Docker Compose, need the NVIDIA Container Toolkit on the
host, and it is missing or not set up for Docker. Older Docker versions
report the same problem as
`could not select device driver "" with capabilities: [[gpu]]`. If you are not the
host's administrator, ask them to fix it. Install the
toolkit and configure Docker for it as the
[NVIDIA Container Toolkit installation guide](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)
describes, then restart Docker (see also
[Docker and local deployment](quickstart/environment.md#docker-and-local-deployment)).

Docker still creates the daemon container. Remove it with
`docker rm gpudogd` before you run the same `docker run --name gpudogd ...`
again; otherwise Docker refuses because the name is in use. A client
started with `--rm` needs nothing.

## `daemon granted device ... outside the requested CUDA view`

The daemon returned an inconsistent grant. The client closes its connection
without starting the command. Install matching, trusted binaries and inspect
the daemon logs; do not bypass the validation.

## `configured device GPU-... was not found`

A UUID under `devices` does not exist or is outside the daemon's CUDA view.
Check both CUDA visibility and Docker device exposure; NVML visibility alone
is not sufficient. On a reload the message includes `when gpudogd started`: the
daemon can manage only GPUs it saw at startup, so restart it when the GPUs
are idle (see [Shutdown](administration/daemon.md#shutdown)).

## My process sees the wrong GPU

*   Do not change `CUDA_VISIBLE_DEVICES` or `CUDA_DEVICE_ORDER` inside your
    program.
*   `-g` takes CUDA numbers; `dash` shows daemon GPU numbers (see
    [GPU numbers](concepts.md#gpu-numbers)). Use UUIDs to compare GPUs between
    environments.

## `CUDA-capable device(s) is/are busy or unavailable`

Your job got its GPUs, but CUDA refused them. This error comes from CUDA, not
from GPUDog. The usual cause is the GPU's compute mode, which you can check
without root:

```shell-session
$ nvidia-smi --query-gpu=index,uuid,compute_mode --format=csv
index, uuid, compute_mode
0, GPU-8f0c3c4e-..., Exclusive_Process
```

The `index` column may differ from both your CUDA number and the daemon GPU
number; find your GPU by the `uuid` column, which matches `CUDA_VISIBLE_DEVICES`
in your job.

*   `Exclusive_Process` allows only one process per GPU. If your job uses the
    GPU from several processes, keep CUDA in one of them, for example by
    doing no CUDA work in data loader workers. A program started without
    `gpudog` may also have taken the GPU first.
*   `Prohibited` allows no process on the GPU. Ask your administrator to
    leave it out of GPUDog.

In any other case, or if you need the GPU from several processes, ask your
administrator; see
[GPU compute modes](administration/unmanaged-processes.md#gpu-compute-modes).

## My GPUs were not released

A reservation lasts as long as any process holds the connection, and child
processes inherit it. If your program started background processes that are
still running, they keep the GPUs reserved until they exit. `gpudog dash`
shows the pid of the `gpudog` process; look for its descendants with
`ps -ef --forest`.

A stopped job (Ctrl-Z) also keeps its GPUs. Check `jobs` in the shell that
started it.

A command that hangs keeps its GPUs indefinitely: the daemon never takes GPUs
back from a running job. Killing only `gpudog` does not help, because the
command keeps the reservation. Find the command under the `gpudog` process
with `ps -ef --forest` and stop it, for example with `kill <pid>`, then stop any
processes it started. You can also ask an administrator; see
[A job holds GPUs too long](administration/daemon.md#a-job-holds-gpus-too-long).

## `gpudog run` keeps waiting after its timeout

The daemon measures `-t`, so a daemon that is stuck cannot end the wait.
If the time has passed and nothing happens, run `gpudog health` in another
terminal:

*   If it fails with `gpudogd did not answer within 10 seconds`, the daemon is
    stuck. Press Ctrl-C to cancel your request and ask the administrator to
    check `journalctl -u gpudogd` (or `docker logs gpudogd`).
*   If it answers, check that the `-t` value is what you meant, and look at
    your request in `gpudog dash`. If the time has clearly passed and the
    request still shows as waiting, press Ctrl-C and ask the administrator
    to check the daemon log
    (see [Request lines](administration/logging.md#request-lines)).

## My request waits although a GPU looks free

A GPU with no reservation is still not granted while any process uses it.
It may also be kept for a request ahead of yours that is waiting for it;
see [The queue](concepts.md#the-queue).
In `gpudog dash`, look for rows on that GPU under `GPU processes` (often
`not managed`), or `(+N not managed)` under `Devices`; see
[Why a request waits](user-guide/dashboard.md#why-a-request-waits-although-a-gpu-looks-free).
A `new grants paused` line that disappears within a moment is a routine
check, not a fault. If it stays, the daemon cannot check GPU usage right now
and grants nothing until it can; running jobs are not affected. Do
not stop other users' processes; wait for them, or request other GPUs. A
`not managed` row can also be a process of a running job that the daemon
lost track of, so check its uid and command first. If the row is a display
server such as `Xorg` or `Xwayland`, that GPU is never granted while it
runs; ask an administrator to leave that GPU out of `devices` (see
[GPUs that drive a display](administration/unmanaged-processes.md#gpus-that-drive-a-display)).
Under WSL2 with WSLg, an `unknown` row that never goes away means that GPU
is never granted; see [WSL2](administration/unmanaged-processes.md#wsl2).

# Clients in separate containers

Clients may run in containers on the **same GPU host** as the daemon. The
supported initial topology uses a rootful Docker engine, no user namespace
separation, and a shared pathname Unix socket. TCP/remote-host access and
rootless/user-remapped deployment are not supported. A client in a
rootless container connects as a subordinate uid, and the daemon counts it
as a user of its own; see
[Security](../administration/security.md#untrusted-input).

The daemon does not start jobs in its container. `gpudog run` starts the
command in the client container, with only the granted UUIDs exported in
`CUDA_VISIBLE_DEVICES`. `-g` numbers are the client container's
[CUDA numbers](../concepts.md#gpu-numbers), and `-n` picks from the GPUs
visible in that container; daemon GPU numbers are never used in requests.

## Images and identity

```shell-session
$ docker build -t gpudogd .
$ docker build --target client -t gpudog-client .
```

The default build remains the daemon-only image. The `client` target contains
`gpudog` on the same Bookworm runtime base. It defaults to UID/GID 65532 for
help; that is **not** a mapping of the invoking host user. Pass the host's
numeric UID/GID with `--user` for every real request. The kernel-reported UID,
not a client-supplied identity field, determines per-user caps across all
host and container clients.

If your host uses supplementary groups for GPU/workload-file access, add
their numeric GIDs with `--group-add`; they are not inherited automatically.
Do not solve permissions by using `--privileged` or running all clients as
root. All UID 0 clients would share the same per-user accounting.

Docker administration is privileged: a user who controls the Docker daemon
can launch processes under other UIDs. `SO_PEERCRED` cannot protect against
that host-administrator capability; do not treat Docker access as an
unprivileged GPU-only permission.

## Start the daemon

Create a root-owned socket directory before launching either container:

```shell-session
$ sudo mkdir -p /run/gpudog
$ sudo chown root:root /run/gpudog
$ sudo chmod 0755 /run/gpudog
$ docker run -d --name gpudogd --userns=host --pid=host --network none --gpus all \
    --mount type=bind,src=/run/gpudog,dst=/run/gpudog \
    gpudogd
```

Both images expose `NVIDIA_DRIVER_CAPABILITIES=compute,utility`. The host
needs the NVIDIA driver and NVIDIA Container Toolkit. A subset may be
selected for the daemon with Docker `--gpus` or config's UUID allow-list.
Only one daemon may manage a given GPU. See
[Running the daemon](../administration/daemon.md).

Use Docker against this host's engine, not a context pointing at another
machine. `--userns=host` explicitly disables Docker user remapping for the
container; it does not itself share a PID namespace. The daemon also uses
`--pid=host` for observation; clients do not need host networking or shared
PID namespaces for `run`, `health` or `dash`.

## Run a job

```shell-session
$ docker run --rm --userns=host --network none \
    --user "$(id -u):$(id -g)" --gpus all \
    --mount type=bind,src=/run/gpudog,dst=/run/gpudog,readonly \
    --mount "type=bind,src=$PWD,dst=/work" --workdir /work \
    gpudog-client run -g 0 -- sh -c 'echo "$CUDA_VISIBLE_DEVICES"'
```

The socket mount must reference the same host directory as the daemon's
mount, though the mount path inside a client can differ if `--socket` is
set accordingly. A read-only bind prevents filesystem changes; it does not
make the socket protocol read-only or prevent reservation requests.

Restrict the client's GPUs with Docker `--gpus '"device=GPU-..."'` or
`CUDA_VISIBLE_DEVICES`. Inside a client exposing only one GPU, `-g 0` names
that GPU even if its daemon GPU number is 1. `-n` picks only GPUs that are
both visible in the container and managed by the daemon. Requests for a GPU
that is hidden or not managed, or made with no GPU visible, fail before the
command runs.

The client image does not include a training framework, Python or a CUDA
toolkit. Add your workload's dependencies, or copy `/usr/bin/gpudog` from
the `client` target into a compatible workload image. The official client
binary uses the Bookworm native runtime; the destination must supply
compatible glibc/libgcc/native libraries and the injected CUDA/NVML driver
libraries. See [Build and runtime requirements](../quickstart/environment.md).

Exit status and signal forwarding are the same as native `run`. Stopping or
killing the client container ends its PID namespace and jobs; once all copies
of the reservation connection close, the daemon releases the GPUs. Do not
mount the host Docker socket into a workload container.

## Dashboard in a private PID namespace

Full `dash` receives host process information from the daemon and works
inside a regular client container:

```shell-session
$ docker run --rm --userns=host --network none \
    --user "$(id -u):$(id -g)" \
    --mount type=bind,src=/run/gpudog,dst=/run/gpudog,readonly \
    gpudog-client dash
```

This shows reservations, queue positions, device metadata and daemon-observed
process rows without querying local NVML or `/proc`. PIDs refer to the
daemon's host observation, not the client's private procfs. Add
`--no-gpu-processes` to hide rows; it does not disable daemon observation or
waiting. Only the daemon shares the host PID namespace. See
[Dashboard](dashboard.md) and
[Unmanaged processes](../administration/unmanaged-processes.md).

## Docker Compose

`packaging/compose.yaml` provides the daemon and an opt-in client profile.
The `gpus` key needs Docker Compose >= 2.30. Export the paths and caller IDs:

```shell-session
$ export GPUDOG_UID="$(id -u)"
$ export GPUDOG_GID="$(id -g)"
$ export GPUDOG_SOCKET_DIRECTORY=/run/gpudog
$ export GPUDOG_WORK_DIRECTORY="$PWD"
$ docker compose -f packaging/compose.yaml --profile client build
$ docker compose -f packaging/compose.yaml up -d gpudogd
$ docker compose -f packaging/compose.yaml run --rm client health
$ docker compose -f packaging/compose.yaml run --rm client run -n 1 -- sh -c 'echo "$CUDA_VISIBLE_DEVICES"'
$ docker compose -f packaging/compose.yaml run --rm client dash --no-gpu-processes
```

`depends_on` starts the daemon but is not a socket-health check. If CUDA/NVML
startup is still in progress, retry `health` before launching a job. The
example deliberately uses no network ports, Docker socket or anonymous socket
volume, and shares the host PID namespace only with the daemon (for process
observation), never with clients. Do not also start the packaged systemd daemon
on this socket/runtime directory.

## Regression checks

```shell-session
$ python3 scripts/container-check.py
$ python3 scripts/container-check.py --gpu --skip-build
```

The default check builds production client/daemon images plus the optional
`test-drivers` target and mounts isolated driver doubles into test containers.
No mock library is shipped in the production images. It needs Docker but no
GPU. `--gpu` uses two real non-MIG GPUs and NVIDIA Container Toolkit; omit
`--skip-build` to rebuild the production images.

Both profiles exercise private client PID namespaces, host-PID daemon
observation, and isolated networks,
CUDA UUID/order/visibility, UID caps across containers, separate UID fairness,
queued-client death, holder-container death, wakeup and release. The real GPU
profile also restricts a client with Docker's device subset. They test actual
container placement and reservation behaviour, not a training framework.
The CPU profile also injects unmanaged usage and observation failures to
verify waiting, host child identity, preserved jobs and recovery. The real
allocator profile requires two unused managed GPUs; it never stops external
processes to make them available.

# GPUDog

GPUDog is a small GPU reservation service for shared Linux machines.
Reservations prevent cooperating users from accidentally sharing a GPU.

*   `gpudogd` arbitrates exclusive reservations, usually as a Docker
    container or a systemd service.
*   `gpudog` waits for a reservation, starts your command with the granted
    UUIDs in `CUDA_VISIBLE_DEVICES`, and keeps the reservation socket open.

```shell-session
$ gpudog run -- python train.py
gpudog: holding gpu(s) 0 (GPU-8f6a1c2d-0000-1111-2222-333344445555)
```

GPUDog is not a batch scheduler like Slurm: no submission scripts are
required. It does not enforce GPU access restrictions. The daemon coordinates
GPUDog reservations, and processes started outside GPUDog
are observed by the daemon and block new allocation while using a managed
GPU. Observation failures pause new grants without stopping running jobs.

## Where to go next

*   [Getting started](getting-started.md): start the daemon and run a command.
*   [Installation](installation.md): Docker, Debian package or source build.
*   [Concepts](concepts.md): reservations, the queue and limits.
*   [User guide](user-guide/run.md): client commands and options.
*   [Administration](administration/daemon.md): run and configure the daemon.
*   [Troubleshooting](troubleshooting.md): errors and recovery.
*   [Developer guide](developer/building.md): Cargo, tests and the protocol.

## System requirements

See [Build and runtime requirements](quickstart/environment.md) for the
per-command library matrix, optional build tools and deployment checks.

*   Linux (x86-64), with the runtime libraries required by your build,
    running natively: under WSL2 the daemon cannot see GPU processes (see
    [WSL2](administration/unmanaged-processes.md#wsl2)).
*   GPU allocation requires an NVIDIA driver providing CUDA (`libcuda.so.1`)
    and NVML (`libnvidia-ml.so.1`). The daemon queries host process metadata;
    `health` and all `dash` views need no client GPU libraries.
*   Docker with the NVIDIA Container Toolkit for container deployment.
*   Clients run on the same host, natively or in containers without user
    namespace separation; see [Container clients](user-guide/container-clients.md).
*   The [release pipeline](developer/releases.md) builds and validates native
    artifacts for glibc >= 2.28; ordinary builds have host-dependent requirements.

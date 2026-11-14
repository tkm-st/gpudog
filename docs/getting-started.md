# Getting started

An administrator starts the daemon once, and then users
run their commands through the client.

## 1. Start `gpudogd`

The daemon manages every GPU, so run it as a container that can see all
devices and that shares its socket directory with the host. Build the
`gpudogd` image first; see [Installation](installation.md#daemon-docker-image).

```shell-session
$ sudo mkdir -p /run/gpudog
$ docker run -d --name gpudogd --pid=host --userns=host --gpus all \
    -v /run/gpudog:/run/gpudog \
    gpudogd
```

`/run/gpudog` must be owned by root and must not be writable by group or
others (`0755`). The daemon refuses to start otherwise; see
[Security model](administration/security.md) for the reason.

On a machine without Docker, run the binary directly as root:

```shell-session
$ sudo gpudogd --socket /run/gpudog/gpudogd.sock
```

See [Running the daemon](administration/daemon.md) for all options.

## 2. Check that the daemon responds

```shell-session
$ gpudog health
gpudogd is responding on /run/gpudog/gpudogd.sock
```

## 3. Run a process

```shell-session
$ gpudog run -- python train.py
gpudog: holding gpu(s) 0 (GPU-8f6a...)
```

If every GPU is busy, `gpudog` waits in the queue and prints its position:

```shell-session
$ gpudog run -n 2 -- python train.py
gpudog: waiting for GPUs (queue position 0)
gpudog: holding gpu(s) 0,1 (GPU-8f6a...,GPU-1c2d...)
```

When the daemon grants the reservation, `gpudog` starts the process with
`CUDA_VISIBLE_DEVICES` set to the granted GPUs. The GPUs are released when the
process exits, and `gpudog` exits with the same status as the process.

## 4. See who is using the GPUs

```shell-session
$ gpudog dash
```

See [Dashboard](user-guide/dashboard.md).

# Getting started

An administrator starts the daemon once, and then users
run their commands through the client.

## 1. Start `gpudogd`

The daemon manages every GPU, so run it as a container that can see all
devices and that shares its socket directory with the host. Build the
`gpudogd` image first; see [Installation](installation.md#daemon-docker-image).

```shell-session
$ docker run -d --name gpudogd --restart unless-stopped --pid=host --userns=host --gpus all \
    -v /run/gpudog:/run/gpudog \
    gpudogd
```

`--restart unless-stopped` starts the daemon again after a reboot. A rootful
Docker engine creates `/run/gpudog` on the host, owned by root with mode
`0755`, whenever it is missing. If your engine does not (for example with
user namespace remapping enabled), add the line
`d /run/gpudog 0755 root root -` in `/etc/tmpfiles.d/gpudog.conf`,
which recreates it at every boot. The directory must be owned by root and
must not be writable by group or others; the daemon refuses to
start otherwise (see [Security model](administration/security.md)).

On a machine without Docker, run the binary directly as root. It creates
`/run/gpudog` itself when the directory is missing, for example after a
reboot:

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

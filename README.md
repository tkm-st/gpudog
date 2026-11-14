# GPUDog

GPUDog is an easy-to-use GPU lock that stops several processes from
accidentally sharing a GPU on a shared Linux machine.

*   `gpudogd`: a daemon that arbitrates exclusive GPU reservations, usually
    run as a Docker container or a systemd service.
*   `gpudog`: the command users put in front of their own command.

```shell-session
$ gpudog run -n 2 -- python train.py
gpudog: holding gpu(s) 0,1 (GPU-8f6a...,GPU-1c2d...)
```

The process starts with `CUDA_VISIBLE_DEVICES` set to the reserved GPU UUIDs.
`run --gpus` takes CUDA numbers (the device numbers in your environment) and
resolves them to UUIDs; CUDA visibility restrictions are honoured by both
client and daemon. The GPUs are released when the process, and anything it
started, exits.
If no GPUs are free, `gpudog` waits in a queue that serves users who use
fewer GPUs first. `gpudog health` checks the daemon, and `gpudog dash` shows
reservations and observed GPU processes.

GPUDog is not a batch scheduler and does not enforce isolation. It
coordinates users who agree to cooperate. The daemon observes GPU processes
without stopping them, waits before allocating a GPU observed in use, and
pauses new grants when observation fails. Running jobs are not revoked.
External processes can still start after a sample; this is not access control.

Changes and known limitations are listed in [`CHANGELOG.md`](CHANGELOG.md).
To report a security vulnerability, see [`SECURITY.md`](SECURITY.md).
Before 1.0, releases make no compatibility promise; upgrade `gpudog` and
`gpudogd` together.

## Documentation

To install a release, see
[From a GitHub release](docs/installation.md#from-a-github-release).

The full documentation is at <https://tkm-st.github.io/gpudog/> and in
[`docs/`](docs/index.md):

*   [Getting started](docs/getting-started.md)
*   [Installation](docs/installation.md) and [concepts](docs/concepts.md)
*   [Build and runtime requirements](docs/quickstart/environment.md)
*   [User guide](docs/user-guide/run.md)
*   [Clients in separate containers](docs/user-guide/container-clients.md)
*   [Administration](docs/administration/daemon.md) and
    [configuration](docs/administration/configuration.md)
*   [Troubleshooting](docs/troubleshooting.md)
*   [Developer guide](docs/developer/building.md) and
    [wire protocol](docs/developer/protocol.md)

To browse it as a website, run `uvx zensical serve` in the repository root.
Build the static site with `uvx zensical build --clean`.

## Requirements

*   Build: Rust >= 1.88, Cargo and a native Linux linker toolchain. No CUDA
    toolkit or NVIDIA driver is needed to compile or run the ordinary tests.
*   GPU work: Linux x86-64 and an NVIDIA driver providing CUDA Driver API
    (`libcuda.so.1`) and NVML (`libnvidia-ml.so.1`), with MIG inspection APIs.
*   Docker deployment: NVIDIA Container Toolkit, GPU exposure and
    `NVIDIA_DRIVER_CAPABILITIES=compute,utility`.

The Dockerfile's default target builds `gpudogd`; `--target client` builds a
client image. Same-host container clients share the Unix socket without user
namespace separation and use the invoking host UID/GID. The daemon needs
`--pid=host` for process-tree observation; clients keep private PID namespaces.
`dash` receives daemon process metadata, and `--no-gpu-processes` hides rows.

The dedicated [release pipeline](docs/developer/releases.md) produces
x86-64 Linux binaries for glibc >= 2.28. Ordinary host/Bookworm builds do not
guarantee that minimum. `health`, `dash` and `run` have different GPU library
requirements. See the [complete matrix](docs/quickstart/environment.md).

## Quick build

GPUDog is a Rust Cargo workspace. Rust stable, rustfmt and Clippy are
configured in `rust-toolchain.toml`.

```shell-session
$ cargo build --locked --release --workspace
$ cargo test --locked --workspace
$ docker build -t gpudogd .
```

With [cargo-deb](https://github.com/kornelski/cargo-deb) installed, build one
Debian package containing both binaries and the systemd unit:

```shell-session
$ cargo deb -p gpudogd --no-build
```

See [Building from source](docs/developer/building.md) for requirements and
verification commands, and [Older versions](docs/developer/architecture.md#older-versions)
for what is kept from earlier protocol versions and configurations.

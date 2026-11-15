# Build and runtime requirements

Build tools and GPU runtime libraries are separate requirements. Installing
Rust or the CUDA toolkit is not a substitute for an NVIDIA driver at runtime.

## Build requirements

| Task | Required tools | GPU/driver needed? |
| -- | -- | -- |
| Build the client, daemon or library crates | Rust >= 1.88, Cargo, a native Linux linker toolchain and glibc development files | No |
| Run the Cargo tests | The build tools, `rustc` on `PATH`, and a writable repository `tmp/` | No; hardware tests may report the driver unavailable |
| Format and lint Rust | rustfmt and Clippy | No |
| Build the daemon image | Docker and access to the build images/dependency sources | No; the Dockerfile supplies its own Rust/native toolchain |
| Build the Debian package | Cargo build tools, cargo-deb, and Debian packaging/binary tools (for example dpkg-dev and binutils) | No |
| Build the documentation | Python, Zensical and preferably uv (`uvx`) | No |
| Lint Markdown | Node.js/npm (`npx`) and markdownlint-cli2 | No |

`rust-toolchain.toml` selects the stable channel with rustfmt and Clippy;
it does not pin an exact compiler release. `Cargo.lock` fixes dependency
versions, so use `--locked`. Crates must be available from the registry or
a populated local cache. A native toolchain can use GCC or Clang as its
linker driver; `nvcc` and the CUDA toolkit are not required.

The current target is Linux x86-64 with the GNU/glibc runtime. The tests use
fake GPUs and a mock CUDA/NVML driver built with Rust; they do not require
root, Docker or a physical GPU. Passing those
tests is not proof of actual GPU/container compatibility.

See [Building from source](../developer/building.md) for commands.

## Runtime requirements

Every binary needs Linux x86-64 and compatible native runtime libraries.
Rust, Cargo and the native compilation tools are not needed to run installed
binaries.
The current GNU builds dynamically use libc, libm, libgcc_s and the ELF
loader. The minimum versions depend on the build environment, not just on
the Rust version.

| Operation | Additional requirements |
| -- | -- |
| Normal `gpudogd` | GPU access, CUDA Driver/NVML including UUID, MIG and process-query APIs, plus host PID/procfs visibility for the observer |
| `gpudog run` | The same driver libraries in the client's environment, visible non-MIG GPUs and access to the daemon socket; these checks also apply to UUID requests |
| `gpudog dash` reservation tables | Access to the daemon socket; no CUDA driver enumeration is performed |
| `gpudog dash` GPU process table | Only the daemon socket; the daemon finds and labels the GPU processes, and the client does not read them itself |
| `gpudog health` | Access to a running daemon's socket; the client itself needs no GPU libraries |
| `--help` / `--version` | Only the binary's native runtime libraries; no daemon or GPU required |
| `gpudogd --num-gpus N` | Native runtime libraries and a suitable socket directory; this explicit testing override bypasses CUDA/NVML/MIG discovery |

The NVIDIA driver supplies `libcuda.so.1`; this is the **CUDA Driver API**,
not the toolkit's CUDA Runtime library (`libcudart`). GPUDog runtime-loads
the driver libraries and does not require a CUDA toolkit to build or run
its own discovery code. It creates no GPU context or device memory merely
to enumerate GPUs. The launched command may separately need a framework,
CUDA Runtime, toolkit or other libraries; GPUDog does not provide them.

MIG instances and visible MIG-enabled GPUs are unsupported. Both must be
rejected before allocation. A driver reporting `NotSupported` for a MIG
query indicates that feature is absent; missing symbols, permission errors
and other inspection failures are not treated as safe GPU visibility.

Managed GPUs normally use the `Default` compute mode; see
[GPU compute modes](../administration/unmanaged-processes.md#gpu-compute-modes).

Set visibility/order restrictions before starting the client or daemon.
CUDA caches its device view per process; library users must not change the
environment after discovery. An empty view, including empty
`CUDA_VISIBLE_DEVICES` or `-1`, cannot be used for a GPU request.

The fake UUIDs created by `--num-gpus` do not enable real GPU work on a
driverless machine. A normal client `run` still validates its own CUDA view
and refuses fictitious devices. Use this override for service/wire tests.

## glibc compatibility

The dedicated [release pipeline](../developer/releases.md) produces x86-64
Linux binaries with a **glibc 2.28 minimum**. It builds against glibc 2.28,
checks required ELF versions/dependencies and runs/tests the exported files
on that minimum runtime. Ordinary host and Bookworm builds still do not
guarantee it; package dependencies depend on the build host.

The daemon image builds on `rust:1-bookworm` and runs on
`debian:bookworm-slim` (glibc 2.36). The container uses its image's native
runtime, not the host client's libc. It still shares the host kernel, and
the NVIDIA driver libraries injected into it must load successfully there.
Running the daemon in Docker does **not** lower the runtime requirements of
a client installed on the host.

Inspect a trusted build before deployment:

```shell-session
$ readelf --version-info target/release/gpudog
$ readelf --version-info target/release/gpudogd
$ dpkg-deb --field target/debian/gpudog_0.0.1-1_amd64.deb Depends
```

Use `python3 scripts/release-check.py` for portable tar/`.deb` artifacts and
minimum-runtime validation. Debian 10's EOL archive is used only for isolated
build/tests, never as the production image. Injected NVIDIA driver libraries
must independently be compatible with the chosen runtime.

## Docker and local deployment

*   Install the NVIDIA driver and NVIDIA Container Toolkit on the host.
    Expose the managed GPUs with Docker `--gpus`.
*   Run the daemon with `--pid=host` for host process-tree observation; do
    not share the clients' PID namespaces. This is observation only, not
    `--privileged` or permission to signal external processes.
*   Expose `NVIDIA_DRIVER_CAPABILITIES=compute,utility`. `compute` supplies
    CUDA driver support; `utility` supplies NVML. The image sets both.
*   Bind-mount the same host socket directory into the daemon. Its directory
    must be owned by root or the daemon's user and not group/world-writable;
    use root-owned `0755` for the standard Docker deployment.
*   Do not use user namespace remapping. Client identity and per-user limits
    depend on the kernel's mapped `SO_PEERCRED` UID. Pass `--userns=host` to
    the daemon and client containers, which opts them out of an engine's
    remapping.
*   Clients run on the same host, natively or in separate containers without
    user namespace separation. Container clients must share the pathname
    socket and run under the caller's host UID/GID. Jobs run in the client's
    environment, not the daemon container. See
    [Container clients](../user-guide/container-clients.md).
*   Install the systemd `.deb` or run a Docker daemon, not both on the same
    socket/runtime directory. Systemd is needed only for the packaged service.

Client and daemon must both speak **protocol v6**. Older peers are refused
at handshake. See [Installation](../installation.md) and
[Running the daemon](../administration/daemon.md).

`dash` receives host process metadata from the daemon, so client PID
namespaces do not need host procfs visibility. `--no-gpu-processes` hides
rows only. Classification is best-effort, but any observed usage blocks new
allocation; observer failure/staleness pauses new grants. See
[Unmanaged processes](../administration/unmanaged-processes.md) and
[Dashboard](../user-guide/dashboard.md).

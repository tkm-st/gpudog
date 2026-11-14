# Installation

Install the client on every GPU user's path and run one daemon per machine.
Each GitHub release provides a tarball and a Debian package for x86-64
Linux. The Cargo workspace also builds both binaries, a Debian package with
a systemd unit, and Docker images: the default target contains only the
daemon, and the `client` target only the client (see
[Container clients](user-guide/container-clients.md)).

## From a GitHub release

Releases are at <https://github.com/tkm-st/gpudog/releases>. Each has these
files, built for x86-64 Linux with glibc 2.28 or later (`ldd --version`
shows yours).

`<version>` is the release tag without its `v`, such as `0.0.1`. For a
prerelease such as `v0.0.2-rc.1`, the package file is
`gpudog_0.0.2-rc.1-1_amd64.deb`, and apt lists it as version
`0.0.2~rc.1-1`.

| File | Contents |
| -- | -- |
| `gpudog-<version>-x86_64-linux-gnu.tar.gz` | `bin/gpudog`, `bin/gpudogd`, `LICENSE` and `THIRD-PARTY-LICENSES`, with no top-level directory. |
| `gpudog_<version>-1_amd64.deb` | Both programs, the systemd unit and `/etc/gpudog/config.toml`, for systems with apt and systemd. See [Debian package](#debian-package). |
| `SHA256SUMS` | Checksums of the tarball, the package and the two programs in the tarball. |
| `dependencies.txt`, `toolchain.txt` | The package's dependencies and the toolchain that built it, for information; not checksummed. |

Download `SHA256SUMS` and the file you need, into a new, empty directory:
the package for the daemon and client on a system with apt, or the tarball
for the client alone. Check them before you unpack or install anything:

```shell-session
$ sha256sum --check --ignore-missing SHA256SUMS
gpudog_0.0.1-1_amd64.deb: OK
```

Every file you downloaded must say `OK` (here, only the package was
downloaded); the command fails if one differs. If it prints no `OK` line at
all, the files are not in this directory: check where you saved them. The
checksums detect damaged downloads, not deliberate replacement: releases
are not signed.

Then install what you downloaded:

*   **Daemon and client, on a Debian-based system.** The package enables
    and starts the daemon, so read [Debian package](#debian-package) first;
    everything there except building it (`cargo build`, `cargo deb`,
    `third-party-licenses.py` and the release pipeline) applies to a
    downloaded package too. Then:

    ```shell-session
    $ sudo apt install ./gpudog_0.0.1-1_amd64.deb
    ```

*   **Client only**, for example next to a
    [Docker daemon](#daemon-docker-image) or on a system without apt:
    unpack the tarball in that directory and install `gpudog`:

    ```shell-session
    $ tar -xzf gpudog-0.0.1-x86_64-linux-gnu.tar.gz
    $ sudo install -m 0755 bin/gpudog /usr/local/bin/gpudog
    ```

    The tarball has no systemd unit or configuration file, so it is not a
    way to run the daemon: run `gpudogd` from the package or in
    [Docker](#daemon-docker-image).

## Daemon: Docker image

The default target of the repository's `Dockerfile` builds an image that
contains only `gpudogd`.
In a clone of the repository, check out the tag of the release you install
the clients from (for example `v0.0.1`; see the
[releases page](https://github.com/tkm-st/gpudog/releases)), so that both
speak the same protocol:

```shell-session
$ git checkout v0.0.1
$ docker build -t gpudogd .
```

The image does not include the CUDA toolkit or the NVIDIA driver. The
NVIDIA container runtime adds CUDA/NVML driver libraries when you start it with
`--gpus`. See [Running the daemon](administration/daemon.md) for the
`docker run` flags.

## Client and daemon: from source

Follow [Building from source](developer/building.md), then install the
binaries:

```shell-session
$ sudo install -m 0755 target/release/gpudog /usr/local/bin/gpudog
$ sudo install -m 0755 target/release/gpudogd /usr/local/bin/gpudogd
```

If you later install the [Debian package](#debian-package), remove these
copies first: they would shadow the package's `/usr/bin` binaries.

Every user who runs GPU work needs `gpudog`, on the host or in a separate
container on that host. Build the Dockerfile's `client` target for a client
image; see [Container clients](user-guide/container-clients.md) for socket,
UID and GPU exposure requirements.

## Debian package

Use the package instead of running a Docker daemon on the same socket.
Package installation enables and starts the systemd service, and that
service owns the lifecycle of `/run/gpudog`. For a Docker deployment,
install only the client, from a
[release tarball](#from-a-github-release) or the source build, instead.

First remove any `gpudog` or `gpudogd` you installed in `/usr/local/bin`
from a tarball or a source build
(`sudo rm -f /usr/local/bin/gpudog /usr/local/bin/gpudogd`): `/usr/local/bin`
usually comes first in `PATH`, so an old copy would shadow the packaged one.

With cargo-deb installed, build and install the package:

```shell-session
$ cargo build --locked --release --workspace
$ cargo deb -p gpudogd --no-build
$ sudo apt install ./target/debian/gpudog_0.0.1-1_amd64.deb
$ systemctl status gpudogd
```

The package installs `/usr/bin/gpudog`, `/usr/bin/gpudogd`,
`/usr/lib/systemd/system/gpudogd.service` and `/etc/gpudog/config.toml`. Its
maintainer scripts enable and start the service; the unit creates
`/run/gpudog` with safe permissions.
Removing the package stops the service, which ends every reservation and
every waiting request: remove it when no job holds or waits for a GPU, with
`sudo apt remove gpudog` (or `sudo apt purge gpudog` to delete
`/etc/gpudog/config.toml` too).

The binaries contain third-party Rust crates. The release tarball and the
release package list them with their licenses in `THIRD-PARTY-LICENSES`
(in the package, `/usr/share/doc/gpudog/THIRD-PARTY-LICENSES`). A package
built with the `cargo deb` commands above does not include it. To produce
it for a source build, and to ship it with any package you pass on, run:

```shell-session
$ python3 scripts/third-party-licenses.py --output THIRD-PARTY-LICENSES
```

It reads the crates cargo has already downloaded. If cargo reports that it
could not find them offline, download them first with
`cargo fetch --locked --target x86_64-unknown-linux-gnu`.

`/etc/gpudog/config.toml` lists every setting, commented out, with its
default (`devices` shows an example), so the daemon starts with the defaults
until you edit it (see [Configuration](administration/configuration.md)). It
is a configuration file of the package:

*   An upgrade keeps your edits, but may ask about them first (see below).
*   `apt remove` keeps it; `apt purge` deletes it, with your edits. If you
    delete the file yourself, upgrades do not bring it back and the daemon
    uses the built-in defaults.
*   The installation asks whether to keep your file in two cases:
    *   A different `/etc/gpudog/config.toml` exists that the package did
        not install: on a first installation, or on an upgrade from a
        package built before it shipped this file.
    *   You edited the file, and the new package changes it too.

    The default answer, `N`, keeps your file and saves the package's version
    next to it as `config.toml.dpkg-dist`. `Y` installs the package's version
    and saves yours as `config.toml.dpkg-old`. Without a terminal to answer
    on, for example in a script, the installation fails with
    `end of file on stdin at conffile prompt`; run it again with the answer,
    giving the path of your package file:

    ```shell-session
    $ sudo apt-get install -o Dpkg::Options::=--force-confold ./gpudog_0.0.1-1_amd64.deb
    ```

Choose either this service or a Docker daemon, not both on the same socket.
Runtime library requirements of ordinary packages depend on the build
environment. For glibc >= 2.28, use the dedicated
[release pipeline](developer/releases.md), then install its exported package:

```shell-session
$ python3 scripts/release-check.py --output tmp/my-release
$ sudo apt install ./tmp/my-release/gpudog_0.0.1-1_amd64.deb
```

For a Docker daemon and native host clients, install only the exported
`tmp/my-release/bin/gpudog`; do not install a second daemon service.

### Upgrading the package

Before upgrading, check with `gpudog dash` who holds or waits for a GPU: after
an upgrade that changes the protocol, the new `gpudog dash` cannot reach the
running daemon. An upgrade may also ask about `/etc/gpudog/config.toml`; see
[Debian package](#debian-package). To upgrade, install the newer package
file the same way as the first one (the package is named `gpudog`):

```shell-session
$ sudo apt install ./gpudog_0.0.2-1_amd64.deb
```

Installing a newer package does not restart a running `gpudogd`, because a
restart loses every reservation (see
[Shutdown](administration/daemon.md#shutdown)). The previous daemon keeps
running, so reservations, running jobs and waiting requests are not affected.
The package says so:

```text
gpudog: gpudogd was not restarted; the version installed before is still running.
gpudog: Clients from this package may not work with it until it restarts.
gpudog: Restart it when no job holds or waits for a GPU (see `gpudog dash`):
gpudog:     sudo systemctl restart gpudogd
```

The new `gpudog` client is installed at once. Run `gpudog health`: if the new
version uses another protocol, it fails with
`protocol version mismatch: daemon speaks protocol version N, client speaks M`,
and so do all new `gpudog` commands until the daemon restarts; `gpudog dash`
shows the same error. Commands that were already running keep their GPUs.

Restart soon, when no job holds a GPU and no request waits for one, or when
every running job already uses its GPUs (see
[Shutdown](administration/daemon.md#shutdown)). A restart ends waiting
`gpudog run` commands with `lost the connection to gpudogd`, and their users
must run them again.

To see which version runs, run the daemon's own executable through `/proc`.
This works even after the upgrade replaced the file:

```shell-session
$ systemctl is-active gpudogd
active
$ sudo /proc/$(systemctl show -p MainPID --value gpudogd)/exe --version
gpudogd 0.0.1 (protocol v6)
```

If `gpudog dash` shows the mismatch, read the
[request lines](administration/logging.md#request-lines) that the running
daemon has logged since it started. `-b` limits the output to this boot, so
an earlier daemon that had the same pid is not included:

```shell-session
$ sudo journalctl -b -u gpudogd _PID=$(systemctl show -p MainPID --value gpudogd)
```

Each client is named by `uid=... gid=... pid=...`. A client with a
`granted gpus=[...]` line and no later `released its reservation` line
still holds those GPUs. A client with an `acquire` line and no later
`granted`, `denied` or `released its reservation` line may still be waiting.
The log can be incomplete. Do not rely on it, and check the GPU processes
with `nvidia-smi` and ask the users before you restart, if:

*   its first line does not end with the daemon's
    `gpudogd ... (protocol vN) starting` line (the journal no longer has it,
    or the daemon is from an earlier development build that never logs it);
*   it contains `log line(s) dropped`; or
*   `sudo journalctl -b | grep -i 'suppressed.*gpudogd'` prints anything:
    journald dropped some of the daemon's lines.

If the service is not active, `MainPID` is `0`: these commands print
nothing or fail. If the service was stopped before the upgrade, the upgrade
starts the new version when the service is enabled and leaves it stopped
when it is disabled; either way it prints no notice. Reinstalling the same
version neither restarts the daemon nor prints a notice.

## Checking the version

Both programs print their version and the wire protocol version:

```shell-session
$ gpudog --version
gpudog 0.0.1 (protocol v6)
```

The client and the daemon must use the same protocol version. If they do not,
the client fails with `protocol version mismatch` when it connects.
`gpudogd --version` reports the installed file, not the daemon that runs; see
[Upgrading the package](#upgrading-the-package) for the running version.

## Compatibility between versions

Releases before 1.0 make no compatibility promise: any release may change
the wire protocol, the configuration file, command-line options, messages or
log lines, including the request lines described above. Each release lists
such changes in its release notes and in
[`CHANGELOG.md`](https://github.com/tkm-st/gpudog/blob/main/CHANGELOG.md).

*   Install the same version of `gpudog` and `gpudogd` everywhere they run,
    including client container images.
*   Read the changelog before upgrading, and check the configuration file
    against it.
*   After upgrading the daemon, restart it when no job holds or waits for a
    GPU (see [Upgrading the package](#upgrading-the-package)).

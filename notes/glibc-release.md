# Minimum-runtime release gotchas

<!-- markdownlint-disable-next-line MD036 -->
_Last verified: 2026-11-02 against the glibc-228 working tree_

## Summary

Portable artifacts come from `Dockerfile.release`, not the normal Bookworm
or host build. It links against glibc 2.28 and validates the exported bytes
in a fresh minimum-runtime container. See `docs/developer/releases.md` for
commands and the guarantee's driver/runtime boundary.

## Details

Copying a fixed Rust installation is not enough: the workspace's
`rust-toolchain.toml` selects `stable`. Set `RUSTUP_TOOLCHAIN=1.88.0` in the
release builder so it does not resolve/download a newer compiler. Rust and
Cargo 1.88.0 plus cargo-deb 3.8.0 were executed inside Debian 10; the tools'
host compatibility is verified, not inferred from Rust's minimum baseline.
Both base image digests are pinned; archived APT metadata expiry is relaxed
without disabling package signature checks. Debian 10 is EOL and all those
stages are isolated build/test environments, never production deployment.

ELF version **needs**, not definitions, determine runtime requirements.
Newer symbols, `GLIBC_PRIVATE`, unknown ABI tags, unexpected native libraries
and wrong architecture/interpreter are rejected. Numeric GCC version needs
are also satisfied by actual execution on the minimum runtime; the ELF
checker alone is not the entire compatibility guarantee.

cargo-deb's auto dependencies omitted `libgcc_s.so.1` despite the exported
ELF requiring GCC 3.0/3.3/4.2.0. Debian 10's `libgcc1` is not Essential.
The release-only `glibc-228` variant explicitly declares
`libgcc-s1 | libgcc1 (>= 1:4.2)`, preserving old and modern package names.
The resulting package also requires `libc6 (>= 2.28)`. Its minimum-runtime
installation and tar/deb binary equality pass. Use `--no-strip` and xz
compression when packaging the existing bytes; separate strip operations
would invalidate byte comparisons and can mask which artifact was tested.

The existing startup-failure test originally waited for daemon exit
before deciding its parent directory was safe. Running as root in Docker
made the daemon start successfully and the test hang forever. It now creates
its own unique 0777 directory and tests a deterministic rejection; never
change permissions on the shared scratch parent to make that test fail.

A 100ms observer can legitimately show an initial `unknown free` frame.
Repeatedly creating a dashboard and sampling only its first frame can
synchronize with observation queries and fail readiness despite later fresh
frames. The harness keeps one bounded dashboard poll until the required
fresh frame appears. Do not bypass freshness or stop unrelated GPU processes
to make allocator tests pass.

## Verification

The minimum builder ran all 373 Debug and 373 Release workspace tests.
Fresh Debian 10 installed the package with service startup suppressed.
Exported binaries passed ELF/checksum/equality gates, CPU container usage/
failure/recovery tests, and real CUDA/NVML cross-container checks on glibc
2.28. The exported executables also ran in the Bookworm CPU harness and
on the newer host. Real training/MIG and booted-systemd lifecycle remain
outside these checks; injected driver libraries must themselves suit the OS.

## Why

Keeping the legacy build environment separate avoids deploying an EOL
distribution just to lower a host binary's ABI requirements. Native linking
plus symbol and real execution gates are stronger than lowering metadata
or merely renaming a newer-runtime build.

## See also

*   `docs/developer/releases.md`.
*   `notes/container-clients.md` and `notes/device-identity.md`.

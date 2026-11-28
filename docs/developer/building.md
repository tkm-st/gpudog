# Building from source

## Build requirements

For ordinary Cargo builds, install:

*   Rust 1.88 or newer and Cargo, preferably via [rustup](https://rustup.rs/).
    `rust-toolchain.toml` selects stable with rustfmt and Clippy, not an exact
    compiler version. `Cargo.lock` pins dependency versions.
*   A native linker/toolchain for Linux x86-64 (for example GCC or Clang),
    with the GNU/glibc development files needed to link executables.
*   Registry access or a local cache containing the locked dependencies.

Additional tools are needed only for their respective tasks:

| Task | Tools |
| -- | -- |
| Debian packaging | [cargo-deb](https://github.com/kornelski/cargo-deb) and Debian packaging/binary tools (for example dpkg-dev and binutils) |
| Docker image | Docker; its builder image supplies the compilation toolchain |
| Rust checks | rustfmt and Clippy (included in the selected rustup components) |
| Documentation | Python and Zensical, preferably through uv/uvx |
| Markdown checks | Node.js/npm and markdownlint-cli2@0.24.0 through npx |

No CUDA toolkit is required. NVML is
loaded at runtime, as is CUDA's `libcuda.so.1`, so binaries do not link the
NVIDIA driver at build time. Discovery creates no CUDA context or device
memory. Workspace dependencies are tokio, clap, cudarc, nvml-wrapper,
nix, signal-hook, toml, bytes, tokio-util, futures-util, anyhow and
thiserror.

An NVIDIA driver and a physical GPU are **not build requirements**. The
ordinary Cargo tests use driver doubles and also run without them. See
[Build and runtime requirements](../quickstart/environment.md) for the
per-command library requirements, Docker capabilities and glibc limitations.

## Build and test

```shell-session
$ cargo build --locked --workspace
$ cargo test --locked --workspace
$ cargo build --locked --release --workspace
$ cargo test --locked --workspace --release
```

The binaries are `target/debug/gpudog`, `target/debug/gpudogd`, and their
`target/release/` equivalents. Use `--target-dir tmp/build-check` for an
isolated build.

Unit tests are in each module; integration tests live in `crates/*/tests/`.
Run a subset with `cargo test --locked -p gpudog-daemon scheduler`. Test the
Release profile for scheduler, service and lifetime changes. Repeat
timing-sensitive service and CLI tests when those paths change.

Most tests use a static GPU inventory and need no GPU, root or Docker.
Real CLI/visibility tests compile a tiny standalone Rust cdylib with mock
CUDA/NVML symbols into `tmp/` and inject it only through subprocess library
paths. The unsafe mock ABI is not linked into any workspace crate or
shipped binary.

The client has one test hook: `GPUDOG_TEST_HOLD_GRACE_MS`, in
milliseconds, shortens the 30-second grace between `SIGTERM` and `SIGKILL`
at the hold limit, so CLI tests finish quickly. Release binaries honour it
too, and it is deliberately left out of the user documentation. A value of
30000 or more, or one that is not a number, is silently ignored, so it
cannot lengthen the grace or let a command outlive its limit; keep it that
way.

NVML tests may either exercise the driver or report that it is unavailable;
a green unit suite alone is not a real GPU/container verification.

## Trying it without a GPU

`gpudogd --num-gpus N` bypasses CUDA/NVML/MIG discovery and creates fake
`GPU-static-N` devices.

```shell-session
$ mkdir -p tmp/run && chmod 0755 tmp/run
$ printf '' > tmp/run/config.toml && chmod 0644 tmp/run/config.toml
$ target/debug/gpudogd --num-gpus 2 \
    --config tmp/run/config.toml --socket tmp/run/gpudogd.sock &
$ target/debug/gpudog health -s tmp/run/gpudogd.sock
```

The `chmod` calls matter under a umask such as `002`, the default for
users on Ubuntu: the daemon refuses a configuration file or socket
directory that group or others can write.

Logs go to stderr, not a log directory. The override fabricates UUIDs, so it
is useful for wire/service tests; a production `run` still needs its real
CUDA view and will not accept fictitious UUIDs as visible devices.

## Running all checks

`scripts/check.sh <tier>` runs the local checks by tier and prints one
`GATE <name>: PASS|FAIL` line per check. CI runs the same steps; only its
Zensical version is pinned more tightly:

| Tier | Runs | Needs |
| -- | -- | -- |
| `quick` | `cargo fmt`, Clippy, build and tests | cargo, a C compiler (`cc`) |
| `full` | `quick`, plus the release build and release tests | as `quick` |
| `release` | the release build and release tests | as `quick` |
| `text` | markdownlint, `scripts/check-repo-text.py` (quoted paths in `AGENTS.md`, and the protocol version stated in the instructions, docs and notes) and `scripts/gen-golden.py --check` (the protocol fixtures match their generator) | npx, python3, git |
| `docs` | `text`, plus the Zensical build | npx, python3, git, uv |

Options: `--target-dir DIR` (for example a directory under `tmp/`),
`--repeat N`, `--test FILE` (one integration test file) and
`--filter NAME`. CI also runs actionlint, `scripts/test-actions.py`,
cargo-deny and the Docker and release checks. Before each commit, run
`scripts/precommit-check.sh` on the staged changes (it needs python3 and
git); `AGENTS.md`, under "Git", says how to install it as a git hook. Before a pull request, run `scripts/check.sh full`, plus `docs` when
Markdown or `docs/` changed, and `cargo deny --locked check` when
dependencies changed.

## Formatting and linting

```shell-session
$ cargo fmt --all --check
$ cargo clippy --locked --workspace --all-targets -- -D warnings
$ npx --yes markdownlint-cli2@0.24.0 "**/*.md" "#target" "#tmp" "#externals" "#build" "#.worktrees"
```

## Dependency policy

`deny.toml` sets which licenses, sources and advisories the locked
dependency graph may have. CI runs the same checks with cargo-deny 0.20.2.
To run them locally, install that version (CI downloads its release
tarball instead) and run all checks, or the ones that do not fetch the
advisory database:

```shell-session
$ cargo install --locked cargo-deny --version 0.20.2
$ cargo deny --locked check
$ cargo deny --locked check bans licenses sources
```

Run it after any change to `Cargo.toml`, `Cargo.lock` or `deny.toml`. It
currently prints four duplicate-version warnings (libloading, syn,
thiserror, thiserror-impl, from nvml-wrapper's older dependencies); they do
not fail the check.

When it fails:

*   **A new license:** read what it requires, then add it to both `[licenses]`
    in `deny.toml` and `ALLOWED` in `scripts/third-party-licenses.py`. CI
    checks that the two lists match.
*   **An advisory** (a vulnerability, or an unmaintained or unsound crate):
    update the dependency to a fixed version (`cargo update -p <crate>`), or,
    if it does not affect GPUDog, add
    `{ id = "RUSTSEC-YYYY-NNNN", reason = "..." }` to `ignore` in
    `deny.toml`. An advisory can appear on any branch, unrelated to its
    change; fix it in its own pull request.
*   **A yanked crate:** update it to a version that is not yanked
    (`cargo update -p <crate>`). Do not add yanked crates to `ignore`.

## GitHub Actions

`.github/workflows/ci.yml` runs on pull requests, pushes to `main`, and
manual dispatch. The release workflow also calls it for the tagged commit.
All jobs use standard GitHub-hosted Ubuntu 24.04 x86-64 runners; no GPU or
self-hosted runner is required.

The independent gates are:

*   Rust formatting and Clippy with warnings denied.
*   The dependency policy in `deny.toml`, checked with cargo-deny:
    licenses, wildcard dependencies and crate sources (duplicate versions
    are only reported as warnings), and RustSec advisories. The advisories
    step reads the RustSec database and the crates.io index at run time, so
    it can fail on an unchanged commit when a new advisory or a yanked crate
    appears.
*   Locked workspace builds and tests in both Debug and Release profiles,
    using the stable toolchain from `rust-toolchain.toml`.
*   Workflow syntax checks with actionlint, release guard tests with a mock
    GitHub CLI, Markdown lint, `scripts/check-repo-text.py`, the protocol
    fixture check (`scripts/gen-golden.py --check`), and a Zensical build.
*   Bookworm production client/daemon container checks with CPU driver doubles.
*   The ELF checker tests and the complete glibc 2.28 release checks, including
    the pinned Rust 1.88.0 build, minimum-runtime installation and CPU container
    checks of the exported artifacts.

Actions are pinned by commit SHA. Validation has read-only repository access
and checkout does not persist credentials. Cargo registry sources are cached,
but build outputs are not. New pushes cancel obsolete CI runs for the same
branch or pull request; release runs are not cancelled this way. Jobs have
explicit time limits.

Verified release artifacts and failure diagnostics under `tmp/containers-*`
are retained for seven days as Actions artifacts. Archive files preserve
executable permissions; loose binaries downloaded from Actions artifacts
need `chmod +x` before execution. These CPU checks do not certify real GPUs,
MIG hardware, or live systemd lifecycle behavior.

Standard hosted runner compute is free for public repositories. Private
repositories consume their owner's included compute allowance, and artifact
storage has separate limits. Configure billing budgets with usage stopping,
not just notifications, if unexpected spending must be prevented. This
workflow does not use larger runners or increase the default cache allowance.

See [Releases](releases.md#publishing-with-github-actions) for tag-driven
publication, including the documentation site's GitHub Pages deploy on
stable tags. Container-registry publication and server deployment are not
configured.

Run the release guard tests locally with `uv run scripts/test-actions.py`
(uv installs the PyYAML it needs).
They execute the actual inline workflow scripts against temporary manifests
and a mock `gh`, checking version mismatch, SemVer prereleases, draft retry,
upload failure and refusal to overwrite a published release. They also
check statically that the Pages jobs keep their permissions, pinned actions
and stable-only deploy. They never call
the GitHub API and do not replace a hosted workflow test.

## Packaging

```shell-session
$ cargo build --locked --release --workspace
$ cargo deb -p gpudogd --no-build
$ docker build -t gpudogd .
$ docker build --target client -t gpudog-client .
```

The Debian package includes both binaries and `gpudogd.service`. The default Docker
image includes only the daemon. See [Installation](../installation.md).
The separate `client` target contains the client, not a workload framework.
Use `python3 scripts/container-check.py` for Docker cross-container tests
without GPUs, or add `--gpu` for two real non-MIG devices. These optional
tests also verify UID caps and connection cleanup across PID namespaces;
they are not part of ordinary `cargo test`.
The package's runtime dependencies are calculated from the build host;
the Bookworm Docker image uses the same base release for builder and
runtime. For glibc 2.28-compatible host binaries/packages, use the dedicated
[release pipeline](releases.md), not either ordinary workflow:

```shell-session
$ python3 scripts/release-check.py
```

## Protocol regression fixtures

`crates/gpudog-core/tests/protocol_golden.rs` checks the v8 wire contract,
and `crates/gpudog-daemon/tests/config_compat.rs` checks config
compatibility. Fixtures are kept in this repository.
`scripts/gen-golden.py` generates the current version's frames with its
own Python encoder, not the Rust codec, so the two check each other. It
writes only the current version's fixtures.

A change to the wire layout, an enum value or the protocol version needs
the maintainer's approval (see "Extra human decisions" in `AGENTS.md`).
Once approved:

1.  Change `PROTOCOL_VERSION` and the codec in
    `crates/gpudog-core/src/protocol/`.
2.  In `scripts/gen-golden.py`, set `VERSION` to the same number and change
    the builders from the layout in [Message types](protocol.md#message-types), not from
    the Rust code. Keep the cases that still test something, and add cases
    for the new fields, including a frame in the previous layout that must
    now be refused.
3.  Run `python3 scripts/gen-golden.py crates/gpudog-core/tests/golden`,
    delete the previous version's `protocol_v*.txt` files, and point
    `protocol_golden.rs` at the new ones (`include_str!`, and `expected()`
    for new or changed messages). Then run
    `python3 scripts/gen-golden.py --check` (also part of the `text` gate
    and CI): it fails while the fixtures are missing or differ from the
    script's output, another version's fixtures remain, or
    `PROTOCOL_VERSION` and `VERSION` disagree.
4.  Add a bullet for the version under
    [Compatibility rules](protocol.md#compatibility-rules), a line to
    `CHANGELOG.md`, and grep `docs/`, `notes/` and `AGENTS.md` for the old
    version number: `scripts/check-repo-text.py` checks only a few phrases.

Compatibility rules summarises what each version changed; the full earlier
layouts and fixtures are in git history.

`config_recorded_results.txt` is a frozen record of what an earlier
loader made of each case in `config_cases.txt`; it is never regenerated.
A deliberate change to how an existing case loads is listed in
`DIVERGENCES` in `config_compat.rs` (which asserts that it still
differs), not edited into the record. New cases go in
`config_cases_rust.txt`, with expectations written by hand in
`config_rust_results.txt`; the cases there were originally written for the
priority keys, which were removed along with the priority in protocol v6.

Only regenerate protocol fixtures for an intentional protocol change, not
to hide a regression. See [Wire protocol](protocol.md).

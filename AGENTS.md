# AGENTS.md

## Repository information

### Current repository state

GPUDog is a Rust Cargo workspace, and this repository is its only
implementation.

Implemented: `gpudogd`, `gpudog run` (`-n/-g/-t`), `health`, terminal
`dash`, config loading (SIGHUP reloads the limits and `devices`; socket
keys need a restart), stderr/journal logging, Debian packaging and the
Docker image. Protocol version is **6**. GPU process observation and
PID/start-time process-tree classification run in the daemon. Observed usage
blocks new grants independently of labels; failures/staleness pause new
allocation without stopping jobs or queue deadlines. `watch_unmanaged_ms`
defaults to 1000 and waiting is always enabled. Clients choose no priority:
the daemon orders the queue by user load.

CUDA-environment indices, UUID-only requests, visibility restrictions and
MIG rejection are implemented by `device_visibility`. It uses CUDA Driver
API enumeration without contexts or device memory, with NVML MIG checks.
Same-host container clients are supported without user namespace separation,
using shared pathname sockets and host UID/GID accounting. Docker's `client`
target and `packaging/compose.yaml` provide deployment examples. Use
`--pid=host` on the daemon for procfs identities; clients keep private PID
namespaces. `dash` consumes daemon metadata, and `--no-gpu-processes` hides
rows only. `Dockerfile.release` and `scripts/release-check.py` build and
validate x86-64 glibc 2.28 tar/deb artifacts; normal Bookworm/host builds do
not guarantee that minimum. Debian 10 stages are build/test only. See
`notes/device-identity.md`. Plans in the maintainer's local `tmp/` are not
current code.

Sources of truth: code, then `docs/` (Zensical; `zensical.toml`), then
`notes/`.

### Layout

```text
Cargo.toml              workspace dependencies, lints and release profile
Cargo.lock              locked dependency graph; keep tracked
CHANGELOG.md            changes per release; its sections become release notes
rust-toolchain.toml     stable channel, rustfmt and Clippy
crates/gpudog-lib/      protocol, scheduler, service, client, config, NVML,
                        GPU processes, logging and dashboard
crates/gpudog/          client binary and CLI integration tests
crates/gpudogd/         daemon binary, integration tests, cargo-deb metadata
packaging/              systemd unit, config.toml template, Debian scripts
scripts/                implementation-independent protocol fixture generator
docs/                   user and developer documentation
notes/                  maintained notes
tmp/                    gitignored scratch and session records
```

Do not delete the user's other checkouts or untracked files as part of
normal work.

### Toolchain and commands

Rust >= 1.88 via rustup (`~/.cargo/bin`), tokio, `nvml-wrapper` (runtime NVML
loading), and nix (`SO_PEERCRED`, signals and fd flags). No CUDA toolkit
is required. cudarc's safe result API runtime-loads
CUDA; Docker must expose `NVIDIA_DRIVER_CAPABILITIES=compute,utility`.
`cargo-deb`, Docker and uv are needed for their respective packaging and
documentation checks, not for ordinary Cargo tests.

`scripts/check.sh` is the single local definition of the gates; its cargo
gates match `.github/workflows/ci.yml` (CI additionally runs actionlint,
`scripts/test-actions.py` and cargo-deny, with pinned tool versions): `quick` (fmt, clippy, build, tests), `full`
(plus release build and tests), `release`, `text` (markdownlint and
`scripts/check-repo-text.py`) and `docs` (`text` plus Zensical). It accepts `--target-dir`, `--repeat N`,
`--test FILE` (one integration test file) and `--filter NAME` (test names), and prints `GATE <name>: PASS|FAIL` lines. When you change a
gate, change `check.sh` and propose the matching CI change to the maintainer. The underlying commands:

```shell
cargo build --locked --workspace
cargo test --locked --workspace
cargo test --locked --workspace --release
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release --workspace
cargo deb -p gpudogd --no-build
docker build -t gpudogd .
docker build --target client -t gpudog-client .
python3 scripts/container-check.py      # optional Docker tests, GPU not required
python3 scripts/container-check.py --gpu --skip-build
python3 scripts/test-release-elf.py
python3 scripts/release-check.py         # glibc 2.28 artifacts and CPU checks
python3 scripts/release-check.py --gpu   # also actual driver/GPU checks
uvx zensical build --clean
npx --yes markdownlint-cli2@0.24.0 "**/*.md" "#target" "#tmp" "#externals" "#build" "#.worktrees"
python3 scripts/check-repo-text.py      # quoted paths in AGENTS.md
scripts/precommit-check.sh               # staged index, before each commit
```

Run Release tests for scheduler, service, watcher and lifetime changes.
Service/CLI tests are timing-sensitive: repeat relevant tests and treat
failures as real. Quote actual results, not a historical test count.
NVML tests may report unavailable hardware; that is not a GPU integration
verification. Packaging depends on the build host's runtime libraries;
ordinary host or Bookworm builds do not guarantee glibc 2.28 compatibility.
Release tooling pins Rust/cargo-deb, overrides floating `stable`, validates
ELF requirements and runs exported bytes on a fresh minimum runtime. Never
deploy the EOL Debian 10 build/test stages.

### Invariants and conventions

*   Requested indices are the client's CUDA numbers. Resolve them through
    `VisibleDevices` before I/O; count requests require non-empty UUID
    candidates. Daemon logical indices remain display/internal metadata.
    The v6 codec refuses `ExplicitList`, so a daemon GPU number is never a
    request key. Children get UUIDs, never indices, in
    `CUDA_VISIBLE_DEVICES`.
*   **The fd is the lock.** Keep the connection alive through the child;
    `SessionGuard::drop` releases reservations even on task cancellation.
*   After spawn, `gpudog run` waits for the command through SIGINT, SIGQUIT
    and SIGHUP, re-raises a child's SIGINT death, and never catches a signal
    ignored on entry (`SigIgn`), so the command inherits SIG_IGN.
*   Never hold the service's `Mutex` across `.await`. Register queued
    `oneshot` waiters under the same lock as scheduler submission.
*   Reloadable limits live in `State.config` under that mutex; read them
    there, not from startup copies. A reload (`Service::reconfigure`) is
    forward only: it never closes a connection or revokes a reservation,
    and denies only waiters the new held cap or `devices` makes impossible,
    through `Scheduler::tick`. `gpudogd` reloads with `Load::Reload`, which
    refuses a missing file and ignored values instead of falling back to
    defaults.
*   Each queue walk (`Scheduler::advance`, shared by `submit` and `tick`)
    expires deadlines, then visits waiters in `(load, arrival)` order,
    computed once per walk. A waiter that cannot be granted reserves its
    `blocking_slots`: if it cannot be met without slots its own uid uses
    or slots busy outside GPUDog, only those; else all usable slots; its uid's
    later requests are skipped. `submit` queues first and then walks, so it
    never disagrees with `tick`. Queue positions shown to clients follow the
    walk order; `index_of` is the private index for removal.
*   The scheduler addresses GPUs by `Slot`, a fixed position in the startup
    inventory; `devices` only marks slots managed, retiring (removed while
    held: kept, never allocated, still shown) or unmanaged. Daemon GPU
    numbers are the shown slots in order, recomputed only by
    `Scheduler::set_managed`; convert to them under the lock, in
    `State::reply` and `snapshot`. Only managed slots are observed. A slot
    newly managed is busy until an observation of the current
    `Observation::generation` succeeds; results of an older one are dropped.
*   NVML/procfs reads and process-tree classification run on a blocking worker
    outside that mutex. Usage blocks allocation regardless of process labels.
    Never clear pause/busy state on query failure or revoke running reservations.
    Static `--num-gpus` inventories bypass observation for wire tests only.
*   NVML and descriptors use RAII/Drop. Workspace `unsafe_code` is denied.
*   Wire strings are `Vec<u8>` and bounds count bytes. Existing layout or
    enum-value changes require a protocol version bump and fixture updates.
*   Sanitize untrusted command text at the daemon boundary and again when
    rendering daemon/process text in the client.
*   Daemon logs use a bounded writer queue to stderr, never blocking the
    async service. `log_directory` and `--log-directory` are obsolete,
    accepted with warnings only; no access/error log files are produced.
*   User-facing behaviour changes must update the matching `docs/` pages.
    A change to the wire protocol, configuration, command-line options,
    messages or log lines also gets a line in the `## [<version>] - Unreleased`
    section of `CHANGELOG.md` (add the section if the top one is dated):
    its release notes promise that every such change is listed.
*   Temporary files belong in `./tmp/`, never `/tmp/`; durable findings
    belong in `notes/`.
*   Markdown must lint against `.markdownlint.json`: `*` bullets with three
    spaces, four-space nesting, and a language on every fenced block.
*   Commit messages are gitmoji-prefixed and imperative (see "Git" below).
    Never use `git add -A` or stage secrets.

## Agent profile

This section is for anyone changing GPUDog, by hand or with any coding
agent: which checks to run, what reviews look for, and the Git rules. No
tool-specific setup is needed; the checks are plain scripts.

Where this section names a role (such as `verifier` or `code-reviewer`) and
your own workflow has no such role, do that step yourself: for example, run
the gates where it says `verifier`, with any directory under `tmp/` as
`tmp/<role>-target`.

### Gates

*   `verifier` runs `scripts/check.sh <tier> --target-dir tmp/<role>-target`.
    Tiers are `quick`, `full` (release; required for scheduler, service,
    watcher and lifetime changes), `release`, `text` (Markdown and
    repository text) and `docs` (`text` plus Zensical).
*   Run `docs` as well when `docs/` changes, and `text` alone for
    instruction-only changes (`AGENTS.md`, `notes/`).
*   Packaging changes (`packaging/`, `[package.metadata.deb]`,
    `scripts/package-release.py`, `scripts/release-check.py` and the
    scripts they call): run the `release` tier, then
    `CARGO_TARGET_DIR=tmp/<role>-target cargo deb -p gpudogd --no-build`
    (the deb assets name `target/release/`, so cargo-deb needs the
    environment variable, and both binaries come from the tier's workspace
    build) and inspect the scripts with `dpkg-deb --control`. The shipped
    deb is the `glibc-228` variant: run
    `python3 scripts/third-party-licenses.py --output tmp/<role>-target/release/THIRD-PARTY-LICENSES`
    and
    `CARGO_TARGET_DIR=tmp/<role>-target cargo deb -p gpudogd --variant glibc-228 --no-build --output tmp/<role>-variant.deb`
    as well, and check its files with `dpkg-deb -c`. Build the variant
    before the base deb: every `cargo deb` run deletes the debs already in
    `<target dir>/debian/`, even with `--output` elsewhere. Neither step runs
    `scripts/package-release.py` or `scripts/release-check.py`: when those
    two, the scripts they call or `Dockerfile.release` change, also run
    `python3 scripts/release-check.py` (Docker, CPU checks), or report that
    it could not run.
*   A test or build that reads a file outside `crates/` (for example
    `include_str!` of `packaging/`) needs that file copied before the
    `cargo` steps in `Dockerfile` and `Dockerfile.release`. Without Docker,
    run `cargo test --no-run` on a `tmp/` copy holding only what those
    stages copy first.
*   Dependency changes (`Cargo.toml`, `Cargo.lock`, `deny.toml`): also run
    `cargo deny --locked check` (cargo-deny 0.20.2, as in CI; see
    `docs/developer/building.md`), or report that it could not run.
*   Select one integration test file with `--test FILE` (for example
    `--test cli`), and repeat timing-sensitive service and CLI tests with
    `--repeat 3` or more. `--filter` matches test names only.
*   CI-only: `scripts/container-check.py` (Docker jobs) and the glibc 2.28
    release checks. They assert message text, so grep them when messages
    change.

### Review focus

*   Code review looks at the GPU-lock invariants above, fd and NVML resource
    safety, tokio concurrency and wire compatibility.
*   Security review covers the `gpudogd` trust boundary: socket/IPC, peer
    credentials, config parsing, process spawning, environment, privilege,
    procfs/NVML input (which also orders the queue) and logging. Fairness rules count per uid, so check anything that lets
    one person act as several uids.
*   Risk-heavy modules: `scheduler`, `service`, `client`, `protocol`, the
    process watcher and the binaries.
*   When a message, log line or other user-visible text changes, grep the
    whole repository for its old wording: tests and CI scripts assert it.
*   Instruction checks: `scripts/check.sh text`.

### Git

*   Run `scripts/precommit-check.sh` after staging and before each commit
    (the maintainer's commit hook runs it automatically). It refuses
    gitlinks, build artifacts, large blobs, likely secrets and conflict
    markers, and runs `scripts/check-repo-text.py`. To run it on every
    commit, link it as the hook:
    `ln -s ../../scripts/precommit-check.sh .git/hooks/pre-commit`.
*   Commit messages start with a gitmoji and use the imperative mood, for
    example `🐛 Release reservations when a session panics`. Common
    prefixes: ✨ feature, 🐛 fix, ♻️ refactor, 📝 docs, ✅ tests, 🔧 config
    or tooling, 🔥 removal. A body only when the reason is not obvious. The remote is
    GitHub `tkm-st/gpudog`; use `gh` for it.
*   Pull requests are written in English (see "Pull request language").
*   The maintainer's agents have standing approval for the following steps after merging to `main`.
    Other branches still need a request. A contributor's agent needs its own developer's request.
    1.  Push `main` with `git push origin main`, fast-forward only. Never
        force-push; history rewrites still need their own approval. If the
        push is rejected as non-fast-forward, stop and ask; do not pull,
        rebase or merge `origin/main`.
    2.  Check the CI run for the pushed commit and report its result, or
        that it is still running or not yet queued:
        `gh run list --commit "$(git rev-parse origin/main)"`. It needs the full
        SHA; a short one lists nothing. A failed run does not trigger a
        revert or another push; report it.
    3.  Start `code-reviewer` in the background on the main checkout at the
        pushed `<sha>` (the same `git rev-parse origin/main`), scoped to the whole codebase (every tracked file,
        not only the merged diff), and wait for it before the final reply.
        Report its Blocking and Non-blocking findings; fixes are new tasks
        in the normal workflow.

### Users

*   End users: researchers sharing GPUs, who run `gpudog run`, `dash` and
    `health`.
*   Administrators who install and configure `gpudogd`.
*   The developer, for agent-instruction changes.
*   User-facing material, for a review from a user's point of view: `docs/`, `README.md`,
    `packaging/` examples, `gpudog --help`, `gpudogd --help`, and built
    binaries when the handoff gives their paths.

### Docs map

*   Readers by section:
    *   `quickstart/`, `user-guide/` and `troubleshooting.md`: researchers
        running jobs.
    *   `administration/` and `advanced/`: whoever installs and configures
        the daemon.
    *   `developer/`: contributors.
    *   `notes/`: agents and maintainers.
    *   Internal names (types, modules, wire fields) are fine in
        `developer/`, `advanced/` and `notes/`, not elsewhere.
*   House style: `docs/developer/documentation.md`. Terms are defined in
    `docs/concepts.md`. Use **CUDA number** for client device numbers and
    **daemon GPU number** for the daemon's numbering. Spell names as
    GPUDog, `gpudog`, `gpudogd`, CUDA and NVML.
*   Where facts live:
    *   CLI text, output lines, exit codes and signals:
        `crates/gpudog/src/` (help in `cli.rs`) and `crates/gpudogd/src/`.
    *   Parsing and limits: `cli_options`, `config`.
    *   Queue behaviour and denial messages: `scheduler`, `service`.
    *   CUDA view errors: `device_visibility`.
    *   Wire values: `protocol`.

### Module map

| Module (`crates/gpudog-lib/src/`) | Responsibility |
| -- | -- |
| `protocol/` (`mod`, `codec`, `tests`) | Wire format v6 and framing |
| `peer_credentials` | `SO_PEERCRED` identity via `nix` |
| `gpu_inventory` | NVML via `nvml-wrapper`, plus a static test double |
| `device_visibility` | Context-free CUDA ordinal/UUID discovery and grant validation |
| `scheduler` | Load-ordered queue and the reservation lifecycle |
| `service` | tokio listener and per-session state machine |
| `client` | Client side of the protocol, `into_reservation` |
| `cli_options`, `config` | Argument parsing, `config.toml` loading |
| `daemon_log` | Non-blocking stderr/journal logging |
| `dashboard` | Terminal rendering for `gpudog dash` |
| `gpu_processes` | Managed-UUID NVML queries and host procfs facts |
| `process_watcher` | PID/start-time descendant cache; usage and labels are separate |

Binaries: `crates/gpudog` (client) and `crates/gpudogd` (daemon, owns the
cargo-deb metadata). Packaging steps verify with
`cargo deb -p gpudogd --no-build` after a release build. Plan bottom-up: `protocol`, `cli_options`, `config` and
the `scheduler` core are testable without a daemon, GPU or sockets.

### Extra human decisions

Whatever tools you use, ask before any of the following: ask the person
you work for, or, when you contribute on your own, the maintainer in the
pull request:

*   a push, pull request or issue, an amend, rebase, reset or tag, or
    removing a worktree whose branch is not merged;
*   a new or upgraded dependency, a toolchain pin or a CI change;
*   a change in user-visible behaviour, CLI flags, config keys or defaults,
    or a wire or file format;
*   a trust-boundary change;
*   weakening a check or removing a hook.

GPUDog adds:

*   Wire-protocol layout, enum values, `PROTOCOL_VERSION` or golden fixtures.

### Improvement log

The maintainer logs instruction changes outside this repository. A
contributor describes them in the pull request instead.

## Tests and compatibility

Unit tests live in modules and `<module>/tests.rs`; integration tests are in
`crates/*/tests/`. Protocol and config fixtures in
`crates/gpudog-lib/tests/golden/` preserve the compatibility contract:
`config_recorded_results.txt` is a frozen record, never regenerated (see
`docs/developer/building.md`). `scripts/gen-golden-v6.py` is the current
fixture encoder, independent of the Rust codec. The v3 to v5 fixtures
document earlier layouts and are not tested; `gen-golden-v6.py` builds on
the v3 to v5 generators, so keep them.
CLI/daemon visibility tests compile a standalone mock driver cdylib into
`tmp/`. Its unsafe mock ABI is not a workspace module or production fallback.
Do not edit fixtures merely to make a changed implementation pass.

## Pull request language

*   Write pull request titles and descriptions in English when creating or
    updating a PR in this repository, regardless of the conversation language.
*   This rule applies to PR content only. Continue responding to the user in
    the language required by the conversation instructions.

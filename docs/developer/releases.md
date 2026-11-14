# glibc 2.28 releases

Use the dedicated release pipeline for **x86-64 Linux with glibc >= 2.28**.
An ordinary host build or the Bookworm Dockerfile does not carry this
minimum-runtime guarantee.

## Build and validate

Requirements: Docker/BuildKit with Linux amd64 support, Python 3, `readelf`
(binutils), `dpkg-deb`, and access to the pinned Rust image, Debian archive
and Cargo registry. CPU checks need a rootful Docker engine, not a GPU.

```shell-session
$ python3 scripts/test-release-elf.py
$ python3 scripts/release-check.py
$ python3 scripts/release-check.py --output tmp/my-release --gpu
```

`--output` must name a new directory under this checkout's `tmp/`. Existing
outputs are not overwritten. Without it, the script creates a unique directory.
Logs and artifacts are retained; only test containers created by the harness
are removed. `--gpu` additionally requires two unused non-MIG GPUs and NVIDIA
Container Toolkit; it never stops external jobs to make a GPU available.

The script:

1.  Builds `Dockerfile.release` with fixed Rust 1.88.0 and cargo-deb 3.8.0,
    with digest-pinned base images, overriding floating `stable`. Native linking
    happens against Debian 10's glibc 2.28 and compiler/runtime libraries.
2.  Rejects a wrong ELF architecture/interpreter, unexpected native libraries,
    unknown version tags or a required `GLIBC_*` version newer than 2.28.
    Requirements, not exported symbol definitions, are inspected.
3.  Runs the workspace's Debug and Release tests in that build environment.
4.  Packages the already-built bytes without cargo-deb stripping them, checks
    tar/`.deb` equality, and records dependencies, toolchain and SHA-256 hashes.
    The `glibc-228` packaging variant explicitly depends on the old/new
    `libgcc_s` runtime packages as well as auto-detected libc requirements.
    The checks also require:
    *   maintainer scripts that never stop or restart `gpudogd` on upgrade
        and print the notice asking for a manual restart;
    *   `/etc/gpudog/config.toml` as the only conffile, identical to
        `packaging/config.toml`, owned by root and mode 0644;
    *   a `Maintainer` with an address and a `Homepage` in the control
        fields;
    *   the full `LICENSE` text in the tar archive and in the `.deb`'s
        `copyright` file, which is `packaging/debian/copyright` in Debian's
        machine-readable format;
    *   a Debian changelog whose top entry is the `.deb`'s version;
    *   a `THIRD-PARTY-LICENSES` file, identical in the tar archive and in
        the `.deb`'s `/usr/share/doc/gpudog/`, that names only crates in
        `Cargo.lock` and includes tokio, libc, unicode-ident, nix and clap.

    `scripts/third-party-licenses.py` writes `THIRD-PARTY-LICENSES` in the
    image from `cargo metadata` and the crates' own license files.
    `scripts/test-third-party-licenses.py` tests it there first. It stops
    the build when a crate ships no license file or uses a license outside
    its `ALLOWED` list (MIT, Apache-2.0, ISC, Unicode-3.0, Unlicense): read
    what the new license requires before adding it there and to the
    `[licenses]` list in `deny.toml`. Packages built with a plain
    `cargo deb` do not contain the file.
5.  Installs the package in a fresh glibc 2.28 container without Rust/build
    dependencies. `policy-rc.d` suppresses service startup during this test.
6.  Mounts the **exported binaries** read-only into the minimum-runtime
    containers and runs the existing cross-container harness: visibility,
    host UID limits, queue/death/release, unmanaged-use waiting, observation
    failures and recovery. Optional real GPU checks do not use driver doubles.

The resulting directory contains:

```text
bin/gpudog
bin/gpudogd
gpudog-<version>-x86_64-linux-gnu.tar.gz
gpudog_<version>-1_amd64.deb
SHA256SUMS
dependencies.txt
toolchain.txt
```

The `.deb` file name uses the workspace version, while its `Version` field
is the Debian form: for `0.0.2-rc.1` the file is
`gpudog_0.0.2-rc.1-1_amd64.deb` and `dpkg-deb --field` shows
`0.0.2~rc.1-1`.

Artifact names follow the workspace package version. Keep the manifest with
the artifacts; checksums detect corruption, not malicious replacement. This
pipeline does not provide artifact signing or publish to a container registry.

## Publishing with GitHub Actions

`.github/workflows/release.yml` runs when a `v*` tag is pushed. The tag must
exactly match `v` followed by `[workspace.package].version` in `Cargo.toml`;
for example, version `0.0.1` requires `v0.0.1`. A SemVer prerelease such as
`0.0.1-rc.1` is published as a prerelease; build metadata alone does not
make a version a prerelease.

Before publishing (steps 1 to 3 are needed once per repository):

1.  Enable GitHub Actions and allow the pinned official checkout, cache,
    Node.js setup, artifact upload and artifact download Actions, plus
    `actions/upload-pages-artifact`, `actions/deploy-pages` and the
    `actions/upload-artifact` version that `upload-pages-artifact` calls:
    the workflow pins `upload-pages-artifact` v5.0.0, which calls
    `upload-artifact` v7.0.0. Check this again whenever that pin changes.
2.  Protect release tags with a repository ruleset so only release maintainers
    can create or modify them. Restrict changes to `.github/workflows/` through
    your normal review policy. A person who can push release tags can trigger
    publication of that commit; the workflow does not require `main` ancestry.
3.  Set up GitHub Pages for the documentation site. Pages needs a public
    repository (a private one needs a paid plan, and its site is still
    public). First, in **Settings > Pages**, set the source to **GitHub
    Actions**; this creates the `github-pages` environment, which by default
    accepts deploys from the default branch only and so refuses a tag's.
    Then, in **Settings > Environments > github-pages**, under **Selected
    branches and tags**, add a rule with ref type **Tag** and pattern `v*`.
    The default-branch rule can stay. The rule also admits prerelease tags;
    the workflow itself skips their deploy.
4.  Update the workspace version in `Cargo.toml`, run `cargo update -w` so
    `Cargo.lock` follows (the documented builds use `--locked`), and commit
    both.
5.  Add an entry for it at the top of `packaging/debian/changelog`, headed
    `gpudog (<deb version>) unstable; urgency=medium`, where the deb
    version is the workspace version with `-1` appended, as cargo-deb
    writes it: `0.0.2` becomes `0.0.2-1`. A prerelease whose part after the
    `-` mixes digits with other characters, such as `0.0.2-rc.1`, gets `~`
    instead of `-` and becomes `0.0.2~rc.1-1`; any other prerelease keeps
    its `-`, so `0.0.2-beta` becomes `0.0.2-beta-1`. List the
    user-visible changes and sign it like the existing entry. With
    devscripts installed (`dch` opens your editor for the change list):

    ```shell
    DEBFULLNAME="Takuma Saito" \
    DEBEMAIL="33742727+tkm-st@users.noreply.github.com" \
    dch -c packaging/debian/changelog -v <deb version> -D unstable
    ```

    The `quick` gate's `cargo test` fails until the header has that version
    and the distribution `unstable` (`dch` without `-D` writes
    `UNRELEASED`).
6.  In `CHANGELOG.md`, rename `## [<version>] - Unreleased` to
    `## [<version>] - YYYY-MM-DD`, and check that it lists every change to
    the protocol, configuration, command-line options, messages and log
    lines. The release notes are this section and the `Compatibility`
    section (`scripts/release-notes.py`); the release workflow stops if the
    version has no dated section. A prerelease such as `0.0.2-rc.1` may use
    the `0.0.2` section while it is still `Unreleased`, and its release
    page then shows that heading. After a release, add
    `## [<next version>] - Unreleased` above it for the changes that follow.
7.  With the second Debian changelog entry, remove the
    `initial-upload-closes-no-bugs` override from
    `packaging/debian/lintian-overrides`; `lintian -I` reports it as
    `unused-override` if it stays.
8.  Merge the intended changes and confirm CI is green. Create a tag at that
    exact commit and push it explicitly:

    ```shell
    git tag v<version> <release-commit>
    git push origin refs/tags/v<version>
    ```

The tag workflow checks the version, calls every gate in `ci.yml` on the
tagged commit, then downloads the verified `release-<commit-sha>` artifact
from that same workflow run. Publication never rebuilds the binaries or
downloads artifacts from a pull request or another workflow run. The job
checks `SHA256SUMS` again before creating a draft GitHub Release with the
notes written from `CHANGELOG.md` by the version job, uploading the tar
archive, the Debian package, `SHA256SUMS`, `dependencies.txt` and
`toolchain.txt`, and publishing it. The binaries are not uploaded on their
own: they ship only in the tar archive and the package, next to `LICENSE`
and `THIRD-PARTY-LICENSES`. Release publication does not automatically
change the `latest` release designation.

The gates include cargo-deny's advisories check, which reads the RustSec
database and the crates.io index at run time. If an advisory or a yanked
crate appears between the merge and the tag, the tag's run fails before
anything is published, and rerunning it fails again. Fix it in a new commit
as [Dependency policy](building.md#dependency-policy) describes, then move
the unpublished tag to that commit, keeping the version:

```shell
# Stop unless this prints "release not found" (or true, for a draft).
gh release view v<version> --json isDraft --jq .isDraft
git push origin :refs/tags/v<version>
git tag --force v<version> <fixed-commit>
git push origin refs/tags/v<version>
```

The tag ruleset from step 2 must let release maintainers delete a release
tag for this. Never move a tag whose release was published, and do not
rerun the old tag's failed workflow run afterwards: it would build the old
commit. Other clones keep the old tag until `git fetch --force --tags`. To make this
less likely, rerun CI on the release commit just before tagging.

Only the publication job receives `contents: write` through the automatic
`GITHUB_TOKEN`; no personal access credential is needed. It does not check
out or execute repository source. All validation jobs have read-only access.
No image is pushed to GHCR and no server is updated.

The documentation site at <https://tkm-st.github.io/gpudog/> is deployed by
the same workflow. A read-only job builds it from the tagged commit, with
the same Zensical version as CI, and uploads it as a Pages artifact. After
the release is published, a separate job deploys that artifact. Only this
job gets the `pages` and `id-token` write permissions that a Pages deploy
needs, and it runs no repository source. For a prerelease the site is built
and uploaded, and the deploy job shows as skipped. The first stable tag is
therefore the first real test of the Pages settings from step 3.

Every stable tag replaces the whole site, and the site always shows the
last stable tag deployed: a tag on an older release line puts that line's
documentation back. Deploys run one at a time; if several stable tags are
pushed in quick succession, a newer deploy waiting for its turn replaces an
older one that is still waiting. Between releases, `docs/` on `main` may
be newer than the site.

The deploy runs after the release is public, so a deploy failure leaves
the release published without updated documentation. Fix the cause (usually
the Pages settings). Then, on the workflow run's page under **Actions**,
choose **Re-run jobs > Re-run failed jobs**, which runs the deploy again
with the same site artifact. Do not choose **Re-run all jobs**: it would run
the publication job again, which refuses the published release, so the
deploy would be skipped. The site artifact is kept for seven days. After
that, a published release has no way to redeploy its site; the next stable
release updates it.

If an upload of release files fails, the release stays a draft. While it
is a draft, rerunning the workflow can replace its attachments and complete
publication; an already published release is never overwritten. The
release artifacts are also kept for seven days; after that, rerun all jobs
of a draft's run to rebuild and revalidate them.
Checksums are rechecked on download but are not a cryptographic signature.

`SHA256SUMS` covers the tar archive, the Debian package and the two
binaries inside the archive (`bin/gpudog`, `bin/gpudogd`), not
`dependencies.txt` or `toolchain.txt`. Users check only the files they
downloaded, with `--ignore-missing` and before unpacking (see
[From a GitHub release](../installation.md#from-a-github-release)). To
verify every file of a release, place
the tar archive, Debian package and `SHA256SUMS` in the same directory, then
extract the archive, which restores `bin/` next to them:

```shell
tar -xzf gpudog-0.0.1-x86_64-linux-gnu.tar.gz
sha256sum --check --strict SHA256SUMS
```

## Separate production and compatibility environments

Debian 10 is end-of-life. Its archived environment is **build/test only**;
do not deploy any `Dockerfile.release` stage as a production daemon. Archived
APT metadata expiry is relaxed, but signature verification remains enabled.
Toolchain copying is tested inside that environment, not assumed from Rust's
baseline alone. Changing pinned tools requires rerunning all release gates.

The regular `Dockerfile` continues to build/run on Bookworm and is the
production container path. Use `--pid=host` for its daemon observer and private
PID namespaces for clients. To retest exports on Bookworm without rebuilding
them, first build the normal check images, then pass the binaries:

```shell-session
$ python3 scripts/container-check.py
$ python3 scripts/container-check.py --skip-build --binaries tmp/my-release/bin
$ python3 scripts/container-check.py --release --skip-build --binaries tmp/my-release/bin
```

The compatibility promise concerns GPUDog's executables and the standard GNU
runtime. CUDA/NVML libraries are loaded at runtime: the NVIDIA driver itself
must support the selected OS/glibc and load correctly there. GPUDog does not
bundle those libraries, a CUDA toolkit or a workload framework.

These checks do not certify actual MIG hardware, training frameworks or
systemd package install/upgrade/remove behavior on a booted minimum system.
The package installation test validates dependency resolution, installed
bytes and scripts with startup suppressed, not a live systemd lifecycle.

#!/usr/bin/env python3
"""Build/export glibc 2.28 releases and test the exported files in containers.

Requires Docker, Python 3, readelf and dpkg-deb. Debian 10 images are isolated
build/test environments, not production images. Results stay under ./tmp/.
"""
import argparse
import gzip
import hashlib
import importlib.util
import re
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.dont_write_bytecode = True


def check_artifacts(out):
    binaries = [out / "bin" / name for name in ("gpudog", "gpudogd")]
    spec = importlib.util.spec_from_file_location("release_elf", ROOT / "scripts/check-release-elf.py")
    elf = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(elf)
    for path in binaries:
        elf.inspect(path)
    debs = list(out.glob("*.deb"))
    archives = list(out.glob("*.tar.gz"))
    if len(debs) != 1 or len(archives) != 1:
        raise RuntimeError("expected exactly one Debian package and tar archive")
    expected = {"bin/gpudog", "bin/gpudogd", debs[0].name, archives[0].name}
    checksums = {}
    for line in (out / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split("  ", 1)
        if name not in expected or name in checksums:
            raise RuntimeError("unexpected/duplicate checksum path: " + name)
        checksums[name] = digest
    if set(checksums) != expected:
        raise RuntimeError("incomplete checksum manifest")
    for name, digest in checksums.items():
        if hashlib.sha256((out / name).read_bytes()).hexdigest() != digest:
            raise RuntimeError("checksum mismatch: " + name)
    with tarfile.open(str(archives[0]), "r:gz") as bundle:
        for binary in binaries:
            member = bundle.getmember("bin/" + binary.name)
            if not member.isfile() or bundle.extractfile(member).read() != binary.read_bytes():
                raise RuntimeError("tar binary mismatch: " + binary.name)
        member = bundle.getmember("LICENSE")
        if not member.isfile() or bundle.extractfile(member).read() != (ROOT / "LICENSE").read_bytes():
            raise RuntimeError("tar LICENSE missing or different from the repository's")
        try:
            notices = bundle.extractfile(bundle.getmember("THIRD-PARTY-LICENSES")).read()
        except (KeyError, AttributeError):
            raise RuntimeError("tar has no THIRD-PARTY-LICENSES file")
    with tempfile.TemporaryDirectory(prefix="deb-check-", dir=ROOT / "tmp") as directory:
        subprocess.run(["dpkg-deb", "--extract", str(debs[0]), directory], check=True)
        for binary in binaries:
            installed = Path(directory) / "usr/bin" / binary.name
            if installed.is_symlink() or installed.read_bytes() != binary.read_bytes():
                raise RuntimeError("deb binary mismatch: " + binary.name)
        check_config_template(Path(directory))
        check_copyright(Path(directory))
        check_changelog(Path(directory), debs[0])
        packaged = Path(directory) / "usr/share/doc/gpudog/THIRD-PARTY-LICENSES"
        if not packaged.is_file() or packaged.read_bytes() != notices:
            raise RuntimeError("deb THIRD-PARTY-LICENSES missing or different from the tar's")
    check_third_party_licenses(notices.decode("utf-8"))
    check_config_entry(debs[0])
    check_maintainer_scripts(debs[0])
    check_control_fields(debs[0])
    dependencies = subprocess.check_output(["dpkg-deb", "--field", str(debs[0]), "Depends"], text=True)
    if "libc6" not in dependencies or "libgcc-s1 | libgcc1 (>= 1:4.2)" not in dependencies:
        raise RuntimeError("release package omits required GNU runtime dependencies")
    if dependencies != (out / "dependencies.txt").read_text():
        raise RuntimeError("package dependency report mismatch")
    print("verified checksums, ELF requirements, tar/deb identical binaries; Depends: " + dependencies.strip(), flush=True)


def check_third_party_licenses(text):
    """Fails unless the notices name only locked crates, including known ones.

    The host has no crate sources to regenerate the file, so this checks it
    against Cargo.lock instead; scripts/third-party-licenses.py fails in the
    image if a crate lacks its license files.
    """
    if not text.startswith("GPUDog third-party licenses\n"):
        raise RuntimeError("THIRD-PARTY-LICENSES lacks its heading")
    locked = set(re.findall(r'^name = "([^"]+)"\nversion = "([^"]+)"', (ROOT / "Cargo.lock").read_text(), re.MULTILINE))
    rule = "=" * 72
    listed = re.findall(r"^{}\n(\S+) (\S+)\nLicense: ".format(rule), text, re.MULTILINE)
    unknown = sorted(set(listed) - locked)
    if unknown:
        raise RuntimeError("THIRD-PARTY-LICENSES names crates not in Cargo.lock: " + repr(unknown))
    names = {name for name, _ in listed}
    missing = sorted({"tokio", "libc", "unicode-ident", "nix", "clap"} - names)
    if missing:
        raise RuntimeError("THIRD-PARTY-LICENSES omits " + ", ".join(missing))


def check_control_fields(deb):
    """Fails unless the deb names a maintainer with an address and a homepage."""
    def field(name):
        return subprocess.check_output(["dpkg-deb", "--field", str(deb), name], text=True).strip()
    # Debian policy requires the maintainer's address.
    maintainer = field("Maintainer")
    if not re.fullmatch(r"[^<>]+ <[^<>@\s]+@[^<>@\s]+>", maintainer):
        raise RuntimeError("deb Maintainer has no name and address: " + maintainer)
    if not field("Homepage"):
        raise RuntimeError("deb has no Homepage")


def check_changelog(root, deb):
    """Fails unless the deb's Debian changelog starts with the package's version."""
    version = subprocess.check_output(["dpkg-deb", "--field", str(deb), "Version"], text=True).strip()
    path = root / "usr/share/doc/gpudog/changelog.Debian.gz"
    if not path.is_file():
        raise RuntimeError("deb has no changelog.Debian.gz")
    with gzip.open(path, "rt") as changelog:
        first = changelog.readline()
    if not first.startswith("gpudog ({}) ".format(version)):
        raise RuntimeError("deb changelog does not start with version " + version + ": " + first.strip())


def check_copyright(root):
    """Fails unless the deb's copyright file carries the full LICENSE text."""
    copyright = (root / "usr/share/doc/gpudog/copyright").read_text()
    if copyright != (ROOT / "packaging/debian/copyright").read_text():
        raise RuntimeError("deb copyright differs from packaging/debian/copyright")
    # Machine-readable format: every line is a field or a continuation line.
    if not copyright.startswith("Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n"):
        raise RuntimeError("deb copyright does not declare the machine-readable format")
    for line in copyright.splitlines():
        if line and not line.startswith(" ") and not re.match(r"[A-Za-z][A-Za-z-]*: ", line):
            raise RuntimeError("deb copyright line is neither a field nor a continuation: " + line)
    if "\nCopyright: 2026 Takuma Saito\n" not in copyright:
        raise RuntimeError("deb copyright does not name the LICENSE's holder")
    # Debian indents the text and writes blank lines as " ."; compare words.
    words = (ROOT / "LICENSE").read_text().split()
    if " ".join(words) not in " ".join(word for word in copyright.split() if word != "."):
        raise RuntimeError("deb copyright omits the LICENSE text")


def check_config_template(root):
    """Fails unless the deb installs packaging/config.toml as a safe config file."""
    installed = root / "etc/gpudog/config.toml"
    if installed.is_symlink() or not installed.is_file():
        raise RuntimeError("deb does not install /etc/gpudog/config.toml as a regular file")
    if installed.read_bytes() != (ROOT / "packaging/config.toml").read_bytes():
        raise RuntimeError("deb config.toml differs from packaging/config.toml")


def check_config_entry(deb):
    """Fails unless the archive itself installs config.toml as root:root 0644."""
    listing = subprocess.check_output(["dpkg-deb", "--contents", str(deb)], text=True)
    entries = [line.split() for line in listing.splitlines() if line.endswith(" ./etc/gpudog/config.toml")]
    # gpudogd refuses a config file not owned by root or writable by group or others.
    if len(entries) != 1 or entries[0][0] != "-rw-r--r--" or entries[0][1] not in ("0/0", "root/root"):
        raise RuntimeError("deb config.toml is not root/root -rw-r--r--: " + repr(entries))


def check_maintainer_scripts(deb):
    """Fails unless an upgrade leaves a running gpudogd alone and says so."""
    with tempfile.TemporaryDirectory(prefix="deb-control-", dir=ROOT / "tmp") as directory:
        control = Path(directory) / "DEBIAN"
        subprocess.run(["dpkg-deb", "--control", str(deb), str(control)], check=True)
        postinst = (control / "postinst").read_text()
        prerm = (control / "prerm").read_text()
        postrm = (control / "postrm").read_text()
        conffiles = (control / "conffiles").read_text().split() if (control / "conffiles").exists() else []
    # A conffile keeps the administrator's edits across upgrades.
    if conffiles != ["/etc/gpudog/config.toml"]:
        raise RuntimeError("deb conffiles are not exactly /etc/gpudog/config.toml: " + repr(conffiles))
    # A restart on upgrade loses every reservation (docs/administration/daemon.md),
    # so neither script may restart gpudogd, and prerm stops it only on remove.
    restarts = ("_dh_action=restart", "invoke restart", "invoke try-restart")
    if any(form in script for form in restarts for script in (postinst, prerm, postrm)):
        raise RuntimeError("maintainer scripts would restart gpudogd on upgrade")
    if "stop gpudogd.service" in prerm and '[ "$1" = remove ]' not in prerm:
        raise RuntimeError("prerm would stop gpudogd on upgrade")
    if "gpudogd was not restarted" not in postinst:
        raise RuntimeError("postinst omits the notice that an upgrade did not restart gpudogd")
    # The notice is about a daemon that was running before the generated code
    # starts a stopped one, so the probe must come first.
    if "deb-systemd-invoke start" not in postinst:
        raise RuntimeError("postinst no longer starts gpudogd; review the upgrade notice")
    if "is-active" not in postinst or postinst.find("is-active") > postinst.find("deb-systemd-invoke start"):
        raise RuntimeError("postinst probes gpudogd after starting it, so its notice can be wrong")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="new export directory under tmp/; default is a unique directory")
    parser.add_argument("--gpu", action="store_true", help="also test the exported files against two real GPUs")
    args = parser.parse_args()
    (ROOT / "tmp").mkdir(exist_ok=True)
    if args.output:
        out = args.output.resolve()
        if ROOT / "tmp" not in out.parents or out.exists():
            parser.error("--output must be a new directory under this repository's tmp/")
    else:
        out = Path(tempfile.mkdtemp(prefix="release-", dir=ROOT / "tmp"))
    try:
        subprocess.run(["docker", "build", "--platform", "linux/amd64", "-f", "Dockerfile.release", "--target", "artifacts", "--output", "type=local,dest=" + str(out), "."], cwd=ROOT, check=True)
        check_artifacts(out)
        command = [sys.executable, str(ROOT / "scripts/container-check.py"), "--release", "--binaries", str(out / "bin")]
        subprocess.run(command, cwd=ROOT, check=True)
        if args.gpu:
            subprocess.run(command + ["--gpu", "--skip-build"], cwd=ROOT, check=True)
    finally:
        print("release artifacts: " + str(out), flush=True)


if __name__ == "__main__":
    main()

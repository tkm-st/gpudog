#!/usr/bin/env python3
"""Package the already-built trusted binaries without changing their bytes."""
import hashlib
import json
import shutil
import subprocess
import tarfile
from pathlib import Path


def main():
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], text=True))
    version = next(package["version"] for package in metadata["packages"] if package["name"] == "gpudogd")
    out = Path("/out")
    (out / "bin").mkdir(parents=True)
    for binary in ("gpudog", "gpudogd"):
        shutil.copy2(Path(metadata["target_directory"]) / "release" / binary, out / "bin" / binary)
    # The glibc-228 variant ships it from target/release/.
    notices = Path(metadata["target_directory"]) / "release" / "THIRD-PARTY-LICENSES"
    subprocess.run(["python3", "scripts/third-party-licenses.py", "--output", str(notices)], check=True)
    deb = out / ("gpudog_{}-1_amd64.deb".format(version))
    subprocess.run(["cargo", "deb", "-p", "gpudogd", "--variant", "glibc-228", "--locked", "--no-build", "--no-strip", "--compress-type", "xz", "--output", str(deb)], check=True)
    archive = out / ("gpudog-{}-x86_64-linux-gnu.tar.gz".format(version))
    with tarfile.open(str(archive), "w:gz") as bundle:
        bundle.add(str(out / "bin"), arcname="bin")
        bundle.add("LICENSE", arcname="LICENSE")
        bundle.add(str(notices), arcname="THIRD-PARTY-LICENSES")
    subprocess.run(["dpkg-deb", "-x", str(deb), "/package"], check=True)
    for binary in ("gpudog", "gpudogd"):
        if (out / "bin" / binary).read_bytes() != (Path("/package/usr/bin") / binary).read_bytes():
            raise RuntimeError("package changed binary bytes: " + binary)
    subprocess.run(["python3", "scripts/check-release-elf.py", "/package/usr/bin/gpudog", "/package/usr/bin/gpudogd"], check=True)
    (out / "dependencies.txt").write_text(subprocess.check_output(["dpkg-deb", "--field", str(deb), "Depends"], text=True))
    (out / "toolchain.txt").write_text(subprocess.check_output(["rustc", "-Vv"], text=True) + subprocess.check_output(["cargo", "deb", "--version"], text=True))
    files = [out / "bin/gpudog", out / "bin/gpudogd", deb, archive]
    (out / "SHA256SUMS").write_text("".join("{}  {}\n".format(hashlib.sha256(path.read_bytes()).hexdigest(), path.relative_to(out)) for path in files))


if __name__ == "__main__":
    main()

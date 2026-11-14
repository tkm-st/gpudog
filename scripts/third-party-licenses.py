#!/usr/bin/env python3
"""Write THIRD-PARTY-LICENSES for the gpudog and gpudogd release binaries.

Lists every third-party crate the two binaries reach through normal
dependencies on x86_64-unknown-linux-gnu, with its license expression and the
license, copying and notice files it ships, copied verbatim (line endings
normalised to LF).

The walk uses `cargo metadata`, which unifies features across the workspace
and records weak-dependency edges (`dep?/feature`), so the list can also name
crates used only to build the binaries, such as procedural macros, or not
linked at all (memchr through winnow). Over-listing is safe for a notices
file; do not narrow it with `cargo tree` without checking what it drops.

It fails instead of writing an incomplete file when a crate ships no license
file, keeps one in a directory, or uses a license outside ALLOWED: a new
license needs a person to decide what it requires.

Runs on the Python 3.7 of the Debian 10 release image.

Usage: third-party-licenses.py [--metadata FILE] [--output FILE]
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = "x86_64-unknown-linux-gnu"
BINARIES = ("gpudog", "gpudogd")
# Keep equal to [licenses].allow in deny.toml; CI checks that they match.
ALLOWED = {"MIT", "Apache-2.0", "ISC", "Unicode-3.0", "Unlicense"}
OPERATORS = {"OR", "AND", "WITH"}
LICENSE_NAME = re.compile(r"^(licen[cs]e|copying|notice|unlicense)", re.IGNORECASE)
RULE = "=" * 72

HEADER = """\
GPUDog third-party licenses

The gpudog and gpudogd binaries are built from GPUDog's own code (MIT, see
LICENSE) and the Rust crates below. Each entry names the crate, its version,
its license expression as the crate declares it and the license files it
ships, copied verbatim; a crate used in two versions appears twice. The list
comes from the dependency graph, so it may also name crates used only to
build the binaries, such as procedural macros.

The Rust standard library (MIT OR Apache-2.0) is linked in as well. The C
library, libgcc_s and NVIDIA's CUDA and NVML libraries are loaded from the
system and are not part of the binaries.
"""


def load_metadata(path):
    if path:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
               "--filter-platform", TARGET]
    try:
        # cargo's own error goes to stderr, above the hint.
        output = subprocess.check_output(command, cwd=str(ROOT))
    except FileNotFoundError:
        raise SystemExit("cargo is not on PATH; install Rust (see docs/developer/building.md)")
    except subprocess.CalledProcessError:
        raise SystemExit(
            "cargo metadata failed (its error is above). If cargo said it could not "
            "find packages offline, download them first with: "
            "cargo fetch --locked --target {}".format(TARGET))
    return json.loads(output.decode("utf-8"))


def third_party_crates(metadata):
    """Packages reached from BINARIES through normal edges, workspace excluded."""
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    workspace = set(metadata["workspace_members"])
    roots = [package_id for package_id in workspace if packages[package_id]["name"] in BINARIES]
    if len(roots) != len(BINARIES):
        raise SystemExit("cannot find the {} packages in the metadata".format(" and ".join(BINARIES)))
    seen = set()
    stack = list(roots)
    while stack:
        package_id = stack.pop()
        if package_id in seen:
            continue
        seen.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            # An edge may carry several kinds; a normal one (kind null) counts.
            if any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                stack.append(dependency["pkg"])
    found = [packages[package_id] for package_id in seen if package_id not in workspace]
    return sorted(found, key=lambda package: (package["name"], package["version"]))


def license_ids(expression):
    """SPDX ids in an expression, accepting the old `/` separator."""
    return {word for word in re.findall(r"[A-Za-z0-9.+-]+", expression) if word not in OPERATORS}


def license_files(package):
    directory = Path(package["manifest_path"]).parent
    paths = []
    for entry in sorted(directory.iterdir()):
        if not LICENSE_NAME.match(entry.name):
            continue
        if entry.is_dir():
            raise SystemExit("{} {} keeps license files in {}/; list them by hand".format(
                package["name"], package["version"], entry.name))
        paths.append(entry)
    declared = package.get("license_file")
    if declared:
        path = directory / declared
        if path not in paths:
            paths.append(path)
    if not paths:
        raise SystemExit("{} {} ships no license file".format(package["name"], package["version"]))
    files = []
    for path in paths:
        text = path.read_bytes().decode("utf-8").replace("\r\n", "\n").rstrip("\n")
        files.append((path.name, text))
    return files


def render(metadata):
    parts = [HEADER]
    for package in third_party_crates(metadata):
        expression = package.get("license")
        if expression:
            unknown = license_ids(expression) - ALLOWED
            if unknown:
                raise SystemExit("{} {} uses {}, which is not in ALLOWED; check what it requires".format(
                    package["name"], package["version"], ", ".join(sorted(unknown))))
        elif not package.get("license_file"):
            raise SystemExit("{} {} declares no license".format(package["name"], package["version"]))
        lines = [RULE, "{} {}".format(package["name"], package["version"]),
                 "License: {}".format(expression or "see the file below")]
        if package.get("repository"):
            lines.append("Repository: {}".format(package["repository"]))
        for name, text in license_files(package):
            lines.extend(["", "--- {} ---".format(name), "", text])
        parts.append("\n".join(lines) + "\n")
    return "\n".join(parts)


def main():
    parser = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    parser.add_argument("--metadata", help="read `cargo metadata` output from FILE instead of running cargo")
    parser.add_argument("--output", help="write to FILE instead of standard output")
    args = parser.parse_args()
    text = render(load_metadata(args.metadata))
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    main()

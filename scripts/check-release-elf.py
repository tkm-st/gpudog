#!/usr/bin/env python3
"""Reject x86-64 GNU release executables requiring glibc newer than 2.28.

Inspect trusted build outputs only. readelf is never a substitute for running
the exported files on the minimum runtime or checking injected GPU drivers.
"""

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

ALLOWED_LIBRARIES = {"libc.so.6", "libm.so.6", "libgcc_s.so.1", "libpthread.so.0", "libdl.so.2", "librt.so.1", "ld-linux-x86-64.so.2"}


def required_versions(text):
    # Read needs, not definitions: a library defining new versions does not
    # necessarily require them. An executable without needs is not our GNU ABI.
    sections = text.split("Version needs section", 1)
    if len(sections) != 2:
        raise ValueError("missing dynamic version requirements")
    return re.findall(r"\bName:\s+(\S+)", sections[1])


def validate_versions(names):
    if not names:
        raise ValueError("empty dynamic version requirements")
    for name in names:
        if name.startswith("GLIBC_"):
            value = name[6:]
            if not re.fullmatch(r"[0-9]+(?:\.[0-9]+)+", value):
                raise ValueError("unsupported glibc requirement: " + name)
            version = tuple(int(part) for part in value.split("."))
            if version > (2, 28):
                raise ValueError("requires glibc newer than 2.28: " + name)
        elif not re.fullmatch(r"GCC_[0-9]+(?:\.[0-9]+)+", name):
            raise ValueError("unexpected native version requirement: " + name)


def inspect(path):
    def readelf(*flags):
        return subprocess.check_output(["readelf", "--wide", *flags, str(path)], text=True, env=dict(os.environ, LC_ALL="C"))
    header = readelf("--file-header")
    if not re.search(r"Class:\s+ELF64\b", header) or not re.search(r"Machine:\s+Advanced Micro Devices X86-64\b", header):
        raise ValueError("not an x86-64 ELF executable: " + str(path))
    program = readelf("--program-headers")
    if "[Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]" not in program:
        raise ValueError("unexpected GNU interpreter: " + str(path))
    libraries = re.findall(r"\(NEEDED\).*\[(.*?)\]", readelf("--dynamic"))
    if "libc.so.6" not in libraries or set(libraries) - ALLOWED_LIBRARIES:
        raise ValueError("unexpected native dependencies: " + repr(libraries))
    versions = required_versions(readelf("--version-info"))
    validate_versions(versions)
    print("{}: x86-64 GNU ABI, glibc <= 2.28; {}".format(path, ", ".join(libraries)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("files", nargs="+", type=Path)
    args = parser.parse_args()
    try:
        for path in args.files:
            inspect(path)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print("release ELF validation failed: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

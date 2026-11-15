#!/usr/bin/env python3
"""Check GPUDog's instructions and documentation against the code.

*   Repository paths quoted in backticks in AGENTS.md must exist.
*   Text that states the current protocol version (for example
    `Protocol version is **6**` or `gpudogd 0.0.1 (protocol v6) starting`)
    must name `PROTOCOL_VERSION` from the protocol module. Mentions of
    earlier versions in other words, such as "since protocol v6", are
    history and are not checked.

Private details (host names, personal paths) are not checked here: stop them
before they are committed, with your own pre-commit hook.

It needs only Python 3, and runs in CI and in `scripts/check.sh text`.

Usage: python3 scripts/check-repo-text.py
Exits non-zero and prints `file:line: message` for each problem.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Paths under these prefixes are checked for existence when quoted.
PATH_PREFIXES = ('scripts/', 'crates/', 'notes/', 'docs/', 'packaging/',
                 '.github/')
# Root files that may be quoted bare.
ROOT_FILES = {'AGENTS.md', 'README.md', 'SECURITY.md', 'CHANGELOG.md',
              'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml',
              'zensical.toml', 'Dockerfile', 'Dockerfile.release',
              '.markdownlint.json', '.gitignore', 'deny.toml'}
# Files whose quoted paths must exist.
PATH_CHECKED = ['AGENTS.md']
# Where the current protocol version is defined, and the files that state it.
PROTOCOL_SOURCE = 'crates/gpudog-core/src/protocol/mod.rs'
VERSION_CHECKED = ['AGENTS.md', 'README.md', 'docs', 'notes']
# Phrases that state the current version; group 1 is the number.
CURRENT_VERSION = [
    re.compile(r'Protocol version is \*\*(\d+)\*\*'),
    re.compile(r'Wire format v(\d+)'),
    re.compile(r'\(protocol v(\d+)\)'),
    re.compile(r'speak \*\*protocol v(\d+)\*\*'),
    re.compile(r'daemon speaks protocol version (\d+)'),
]
problems = []


def problem(path, line, message):
    problems.append(f'{path.relative_to(ROOT)}:{line}: {message}')


def strip_fences(lines):
    """Yield (number, line, in_fence) for each line."""
    in_fence = False
    for number, line in enumerate(lines, 1):
        if line.lstrip().startswith('```'):
            in_fence = not in_fence
            continue
        yield number, line, in_fence


def check_paths(path):
    for number, line, in_fence in strip_fences(path.read_text().splitlines()):
        if in_fence:
            continue
        for quoted in re.findall(r'`([^`\n]+)`', line):
            first = quoted.strip().split()[0] if quoted.strip() else ''
            candidate = re.sub(r':\d+(:\d+)?$', '', first.rstrip('.,;:)')).removeprefix('./')
            candidate = candidate.split('#')[0]
            if not (candidate.startswith(PATH_PREFIXES) or candidate in ROOT_FILES):
                continue
            if re.search(r'[<>*{}$]|\.\.\.', candidate):
                continue
            if not (ROOT / candidate).exists():
                problem(path, number, f'path does not exist: {candidate}')


def protocol_version():
    source = ROOT / PROTOCOL_SOURCE
    found = re.search(r'pub const PROTOCOL_VERSION: u32 = (\d+);', source.read_text())
    if not found:
        problem(source, 0, 'cannot find PROTOCOL_VERSION')
        return None
    return int(found.group(1))


def version_files():
    for name in VERSION_CHECKED:
        path = ROOT / name
        if path.is_dir():
            yield from sorted(path.rglob('*.md'))
        elif path.exists():
            yield path


def check_versions(path, current):
    # Fenced blocks count too: sample output states the version as well.
    for number, line in enumerate(path.read_text().splitlines(), 1):
        for pattern in CURRENT_VERSION:
            for found in pattern.finditer(line):
                if int(found.group(1)) != current:
                    problem(path, number,
                            f'states protocol version {found.group(1)}, '
                            f'but {PROTOCOL_SOURCE} has {current}')


def main():
    checked = [ROOT / name for name in PATH_CHECKED if (ROOT / name).exists()]
    for path in checked:
        check_paths(path)
    current = protocol_version()
    versioned = list(version_files()) if current is not None else []
    for path in versioned:
        check_versions(path, current)
    for line in problems:
        print(line)
    if problems:
        print(f'{len(problems)} problem(s) in repository text')
        return 1
    print(f'repository text OK: {len(checked)} files path-checked, '
          f'{len(versioned)} files version-checked')
    return 0


if __name__ == '__main__':
    sys.exit(main())

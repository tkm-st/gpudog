#!/usr/bin/env python3
"""Check that repository paths quoted in GPUDog's instructions exist.

Repository paths quoted in backticks in AGENTS.md must exist.

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


def main():
    checked = [ROOT / name for name in PATH_CHECKED if (ROOT / name).exists()]
    for path in checked:
        check_paths(path)
    for line in problems:
        print(line)
    if problems:
        print(f'{len(problems)} problem(s) in repository text')
        return 1
    print(f'repository text OK: {len(checked)} files path-checked')
    return 0


if __name__ == '__main__':
    sys.exit(main())

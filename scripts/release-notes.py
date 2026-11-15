#!/usr/bin/env python3
"""Print the GitHub release notes for VERSION from CHANGELOG.md.

The notes are the changelog's Compatibility section, the section for the
version, and a fixed note on what the release checks did not cover. GitHub
shows every line break in a release body, so the lines of each paragraph
and list item are joined. A release fails here rather than publish notes
that list nothing:

*   A release needs a `## [VERSION] - YYYY-MM-DD` section.
*   A prerelease (`X.Y.Z-rc.1`) may instead use the section of X.Y.Z, even
    while that is still `Unreleased`.

Usage: release-notes.py VERSION [CHANGELOG]
"""
import re
import sys
from pathlib import Path

FOOTER = (
    "Linux x86-64 tarball and Debian package for glibc >= 2.28. CPU "
    "compatibility checks passed; this is not real GPU validation. See "
    "[Installing from a GitHub release](https://github.com/tkm-st/gpudog/blob/main/docs/installation.md#from-a-github-release) "
    "for how to check and install these files."
)


# A line that starts its own block: a heading, a list item, a table row, a
# quote, a rule or setext underline, a fence, HTML or a link reference
# definition. Anything else continues the line above it.
BLOCK_START = re.compile(
    r"\s*(#|[*+-]\s|\d+[.)]\s|\||>|```|~~~|<|\[[^\]]+\]:"
    r"|(-{3,}|={3,}|\*{3,}|_{3,})\s*$)"
)

# A line that nothing continues: a heading, a table row, HTML, a link
# reference definition or a rule.
SINGLE_LINE = re.compile(r"\s*(#|\||<|\[[^\]]+\]:|(-{3,}|={3,}|\*{3,}|_{3,})\s*$)")


def unwrap(text):
    """Joins the hard-wrapped lines of paragraphs and list items. Fenced
    code, tables, HTML, reference definitions and hard breaks (two trailing
    spaces or a backslash) are left as they are."""
    out = []
    fence = None
    for line in text.splitlines():
        marker = line.lstrip()[:3]
        if marker in ("```", "~~~") and fence in (None, marker):
            fence = None if fence else marker
            out.append(line)
            continue
        previous = out[-1] if out else ""
        joinable = (
            fence is None
            and previous.strip()
            and line.strip()
            and not BLOCK_START.match(line)
            and not SINGLE_LINE.match(previous)
            and not previous.endswith(("  ", "\\"))
        )
        if joinable:
            out[-1] = out[-1].rstrip() + " " + line.strip()
        else:
            out.append(line)
    return "\n".join(out)


def sections(text):
    """Maps each `## ` heading line to its body, in file order."""
    found = {}
    heading = None
    for line in text.splitlines():
        if line.startswith("## "):
            heading = line[3:].strip()
            found[heading] = []
        elif heading is not None:
            found[heading].append(line)
    return {heading: "\n".join(body).strip() for heading, body in found.items()}


def version_section(found, version):
    """Returns (heading, body) for VERSION, or raises SystemExit."""
    released = re.compile(r"\[{}\] - \d{{4}}-\d{{2}}-\d{{2}}$".format(re.escape(version)))
    for heading, body in found.items():
        if released.fullmatch(heading):
            return heading, body
    base = version.split("+", 1)[0].split("-", 1)[0]
    if base != version.split("+", 1)[0]:
        prefix = "[{}] - ".format(base)
        for heading, body in found.items():
            if heading.startswith(prefix):
                return heading, body
    if any(heading.startswith("[{}] - ".format(version)) for heading in found):
        hint = "replace Unreleased with the release date"
    else:
        hint = "add the section, with the release date"
    raise SystemExit(
        "CHANGELOG.md has no '## [{}] - YYYY-MM-DD' section; {}".format(version, hint)
    )


def notes(text, version):
    found = sections(text)
    compatibility = found.get("Compatibility")
    if not compatibility:
        raise SystemExit("CHANGELOG.md has no '## Compatibility' section")
    heading, body = version_section(found, version)
    if not body:
        raise SystemExit("CHANGELOG.md section '{}' is empty".format(heading))
    return "## Compatibility\n\n{}\n\n## {}\n\n{}\n\n---\n\n{}\n".format(
        unwrap(compatibility), heading, unwrap(body), FOOTER
    )


def main():
    if len(sys.argv) not in (2, 3) or sys.argv[1] in ("-h", "--help"):
        raise SystemExit(__doc__.strip().splitlines()[-1])
    path = Path(sys.argv[2]) if len(sys.argv) == 3 else Path(__file__).resolve().parent.parent / "CHANGELOG.md"
    sys.stdout.write(notes(path.read_text(encoding="utf-8"), sys.argv[1]))


if __name__ == "__main__":
    main()

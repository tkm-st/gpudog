#!/usr/bin/env bash
#
# Pre-commit safety check for GPUDog.
#
# Verifies the staged index before a commit is created. Exits non-zero and
# explains the fix when something unsafe is staged.
#
# Usage (after staging; the maintainer's commit hook also runs it):
#   scripts/precommit-check.sh
#
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

fail=0
note() { printf '  %s\n' "$*"; }
err() {
    printf '\nBLOCKED: %s\n' "$1"
    fail=1
}

staged=$(git diff --cached --name-only)

if [ -z "$staged" ]; then
    echo "Nothing staged. Stage specific paths with 'git add <path>'."
    exit 1
fi

# 1. Embedded git repos (gitlinks). This repository has no submodules;
#    committing a nested checkout creates a gitlink with no .gitmodules
#    entry, which no clone can resolve.
gitlinks=$(git diff --cached --raw | awk '$2 == "160000" { print $NF }')
if [ -n "$gitlinks" ]; then
    err "embedded git repository staged (gitlink, mode 160000)"
    note "$gitlinks"
    note ""
    note "Nested checkouts stay untracked; this repository has no submodules."
    note "Fix:  git rm --cached <path>"
fi

# 2. Build output and other artifacts that must never be tracked. Deletions
#    are skipped: removing a tracked artifact is the fix, not the problem.
added=$(git diff --cached --name-only --diff-filter=d)
while IFS= read -r path; do
    case "$path" in
        target | target/* | */target/* | build/* | externals/* | compile_commands.json | .cache/* | .vscode/* | tmp/* | .env)
            err "artifact staged: $path"
            note "Fix:  git restore --staged '$path'"
            ;;
    esac
done <<<"$added"

# 3. Large blobs. Nothing in this repo's source should approach this.
while IFS= read -r path; do
    [ -f "$path" ] || continue
    size=$(stat -c %s "$path" 2>/dev/null || echo 0)
    if [ "$size" -gt 1048576 ]; then
        err "large file staged: $path ($((size / 1024)) KiB)"
        note "Confirm this belongs in git history; it cannot be removed later without a rewrite."
    fi
done <<<"$staged"

# 4. Obvious secrets. A coarse net, not a guarantee. The token pattern has
#    matched an ordinary Rust variable named `token` before; rename the
#    variable rather than bypassing the check.
if git diff --cached -U0 | grep -nEi \
    '^\+.*(BEGIN [A-Z ]*PRIVATE KEY|aws_secret_access_key|api[_-]?key["'\'':= ]|password["'\'':= ]|token["'\'':= ][^"]{12,})' \
    >/dev/null 2>&1; then
    err "possible secret in staged diff"
    git diff --cached -U0 | grep -nEi \
        '^\+.*(BEGIN [A-Z ]*PRIVATE KEY|aws_secret_access_key|api[_-]?key["'\'':= ]|password["'\'':= ]|token["'\'':= ][^"]{12,})' \
        | sed 's/^/  /'
    note "Remove it, then rotate the credential if it ever left this machine."
fi

# 5. Conflict markers.
if git diff --cached -U0 | grep -nE '^\+(<{7}|>{7}) ' >/dev/null 2>&1; then
    err "merge conflict marker in staged diff"
fi

# 6. Markdown lint, if the repo's config and a runner are both present.
md=$(printf '%s\n' "$staged" | grep '\.md$' || true)
if [ -n "$md" ] && [ -f .markdownlint.json ] && command -v npx >/dev/null 2>&1; then
    if ! npx --no-install markdownlint-cli2 $md >/dev/null 2>&1; then
        if npx --no-install markdownlint-cli2 --help >/dev/null 2>&1; then
            printf '\nWARNING: markdownlint reported issues:\n'
            npx --no-install markdownlint-cli2 $md 2>&1 | sed 's/^/  /'
            note "Not blocking, but fix before pushing."
        fi
    fi
fi

# 7. Repository text: quoted paths in AGENTS.md.
#    It reads the working tree, so keep unrelated unstaged edits out of the way.
if ! command -v python3 >/dev/null 2>&1; then
    err "python3 is required for scripts/check-repo-text.py"
elif ! out=$(python3 scripts/check-repo-text.py 2>&1); then
    err "repository text check failed"
    printf '%s\n' "$out" | sed 's/^/  /'
    note "Fix the text, or update scripts/check-repo-text.py if the rule is wrong."
fi

if [ "$fail" -ne 0 ]; then
    printf '\nCommit aborted. Resolve the above, then re-run.\n'
    exit 1
fi

echo "Staged changes look safe:"
printf '%s\n' "$staged" | sed 's/^/  /'
exit 0

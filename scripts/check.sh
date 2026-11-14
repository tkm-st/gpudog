#!/usr/bin/env bash
#
# Single local definition of the GPUDog quality gates. The cargo gates match
# .github/workflows/ci.yml; `text` and `docs` run the same markdownlint,
# repository text check and documentation build as CI, but not CI's
# actionlint, scripts/test-actions.py or cargo-deny, and Zensical is unpinned.
# Contributors and tools call this script instead of restating commands.
#
# Usage:
#   scripts/check.sh <quick|full|release|text|docs> [--target-dir DIR] [--repeat N] [--test FILE] [--filter NAME]
#
#   The tier is required and comes first.
#
#   quick    fmt, clippy (-D warnings), debug build and tests
#   full     quick plus release build and release tests (scheduler/service/
#            watcher/lifetime changes)
#   release  release build and tests only
#   text     markdownlint and scripts/check-repo-text.py
#   docs     text plus the documentation build
#
#   --target-dir DIR  cargo target directory; subagents must pass their own,
#                     e.g. tmp/verifier-target
#   --repeat N        run the test step N times (timing-sensitive changes)
#   --test FILE       run only one integration test file, e.g. --test cli for
#                     crates/gpudog/tests/cli.rs (cargo test --test FILE)
#   --filter NAME     forwarded to `cargo test` as a test-name filter; it
#                     matches test names, not files, so use --test for a file
#
# Prints one `GATE <name>: PASS|FAIL` line per step and exits non-zero if any
# step failed. Test output keeps the `test result:` lines for reporting.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

usage() {
    echo "usage: scripts/check.sh <quick|full|release|text|docs> [--target-dir DIR] [--repeat N] [--test FILE] [--filter NAME]" >&2
    exit 2
}

[ $# -gt 0 ] || usage
tier=$1
shift
case "$tier" in
    quick | full | release | text | docs) ;;
    -h | --help)
        echo "usage: scripts/check.sh <quick|full|release|text|docs> [--target-dir DIR] [--repeat N] [--test FILE] [--filter NAME]"
        exit 0
        ;;
    agent) echo "unknown tier: agent (renamed to text)" >&2; usage ;;
    *) echo "unknown tier: $tier" >&2; usage ;;
esac
target=()
repeat=1
filter=()
only_test=()
while [ $# -gt 0 ]; do
    case "$1" in
        --target-dir | --repeat | --filter | --test)
            [ $# -ge 2 ] && [ -n "$2" ] || { echo "$1 needs a value" >&2; usage; }
            case "$1" in
                --target-dir) target=(--target-dir "$2") ;;
                --repeat) repeat=$2 ;;
                --filter) filter=("$2") ;;
                --test) only_test=(--test "$2") ;;
            esac
            shift 2
            ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done
if ! [[ "$repeat" =~ ^[1-9][0-9]*$ ]]; then
    echo "--repeat must be a whole number >= 1" >&2
    usage
fi

# Fail once, clearly, when a required tool is missing.
case "$tier" in
    text) needed=(npx python3) ;;
    docs) needed=(npx python3 uvx) ;;
    *) needed=(cargo cc) ;;  # cc links build scripts and proc macros
esac
for tool in "${needed[@]}"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        hint="install it or rely on CI"
        [ "$tool" = cc ] && hint="install build-essential or rely on CI"
        echo "GATE toolchain: FAIL ($tool not found; $hint)"
        echo "RESULT: FAIL"
        exit 1
    fi
done

failed=0
gate() {
    local name=$1
    shift
    echo "::: $name: $*"
    if "$@"; then
        echo "GATE $name: PASS"
    else
        echo "GATE $name: FAIL"
        failed=1
    fi
}

tests() {
    local profile=("$@")
    local i
    for ((i = 1; i <= repeat; i++)); do
        gate "test${profile[*]:+ ${profile[*]}} (run $i/$repeat)" \
            cargo test --locked --workspace "${profile[@]}" "${target[@]}" "${only_test[@]}" "${filter[@]}"
    done
}

lint_markdown() {
    npx --yes markdownlint-cli2@0.24.0 "**/*.md" "#target" "#tmp" "#externals" \
        "#build" "#.worktrees" "#site" "#.venv"
}

case "$tier" in
    quick | full)
        gate fmt cargo fmt --all --check
        gate clippy cargo clippy --locked --workspace --all-targets "${target[@]}" -- -D warnings
        gate build cargo build --locked --workspace "${target[@]}"
        tests
        if [ "$tier" = full ]; then
            gate "build --release" cargo build --locked --workspace --release "${target[@]}"
            tests --release
        fi
        ;;
    release)
        gate "build --release" cargo build --locked --workspace --release "${target[@]}"
        tests --release
        ;;
    text | docs)
        gate markdownlint lint_markdown
        gate repo-text python3 scripts/check-repo-text.py
        if [ "$tier" = docs ]; then
            gate zensical uvx zensical build --clean
        fi
        ;;
esac

if [ "$failed" -ne 0 ]; then
    echo "RESULT: FAIL"
    exit 1
fi
echo "RESULT: PASS"

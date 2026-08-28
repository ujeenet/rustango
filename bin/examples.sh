#!/usr/bin/env bash
#
# Build + test the example apps under crates/rustango/examples/, one at a time.
#
#   bin/examples.sh --shard middleware      # every middleware_* example
#   bin/examples.sh --all                   # all themed examples
#   bin/examples.sh middleware_cors ...     # named examples
#   bin/examples.sh --list                  # print what would run, build nothing
#
# Why a script rather than `cargo test --workspace`: every example is a SEVERED
# workspace (a bare `[workspace]` in its Cargo.toml), so the root workspace
# cannot see them. That severing is deliberate — each example must be
# copy-pasteable out of the repo as a self-contained project.
#
# Disk is the constraint that shapes this. One example's own target/ is ~1.3 GB,
# and there are enough examples that materialising them all at once would need
# well over 100 GB. Two things keep it bounded:
#
#   1. ONE SHARED target dir (CARGO_TARGET_DIR) across every example. Sharing is
#      an env var, not a workspace change, so the crates stay severed while
#      rustango + sqlx compile once per distinct feature set instead of once per
#      example. This is the difference between minutes and hours.
#   2. `cargo clean -p <example>` after each one, dropping that example's own
#      artifacts while keeping the shared dependency build. Peak disk is
#      therefore "dependencies + one example", not "dependencies + N examples".
#
# Incremental compilation is off for the same reason the root workspace turns it
# off (see the comment on [profile.dev] in the root Cargo.toml): the incremental
# cache balloons target/ by GiBs for a cache that rarely hits in CI.
#
# Set EX_TARGET to relocate the shared target dir — e.g. EX_TARGET=$(mktemp -d)
# on a machine where the repo lives on a small volume.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXAMPLES_DIR="$REPO_ROOT/crates/rustango/examples"

# Theme prefixes, in the order the CI matrix shards them.
THEMES=(middleware orm models api auth views infra files tenancy platform)

export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_TARGET_DIR="${EX_TARGET:-$REPO_ROOT/target-examples}"

usage() {
    sed -n '3,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

# Examples belonging to a theme, as bare directory names. Empty (not an error)
# when a theme has no examples yet — batches land one PR at a time.
theme_examples() {
    local theme="$1"
    find "$EXAMPLES_DIR" -mindepth 1 -maxdepth 1 -type d -name "${theme}_*" \
        -exec basename {} \; 2>/dev/null | sort
}

all_examples() {
    local theme
    for theme in "${THEMES[@]}"; do
        theme_examples "$theme"
    done
}

# --------------------------------------------------------------- arg parsing

targets=()
list_only=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --shard)
            [[ $# -ge 2 ]] || { echo "error: --shard requires a theme name" >&2; exit 2; }
            # shellcheck disable=SC2207
            targets+=($(theme_examples "$2"))
            shift 2
            ;;
        --all)
            # shellcheck disable=SC2207
            targets+=($(all_examples))
            shift
            ;;
        --list)  list_only=1; shift ;;
        -h|--help) usage 0 ;;
        -*) echo "error: unknown flag \`$1\`" >&2; usage 2 ;;
        *)  targets+=("$1"); shift ;;
    esac
done

[[ ${#targets[@]} -gt 0 ]] || { echo "error: nothing selected (try --all)" >&2; usage 2; }

if [[ $list_only -eq 1 ]]; then
    printf '%s\n' "${targets[@]}"
    exit 0
fi

# --------------------------------------------------------------------- run

# Peak disk of the shared target dir, so a run reports the number that matters
# rather than leaving it to be discovered when a build dies at link time.
peak_kb=0
record_peak() {
    local kb
    kb=$(du -sk "$CARGO_TARGET_DIR" 2>/dev/null | cut -f1 || echo 0)
    [[ $kb -gt $peak_kb ]] && peak_kb=$kb
    return 0
}

failed=()
started=$SECONDS

echo "==> ${#targets[@]} example(s), target dir: $CARGO_TARGET_DIR"

for ex in "${targets[@]}"; do
    dir="$EXAMPLES_DIR/$ex"
    if [[ ! -d $dir ]]; then
        echo "!! $ex: no such example" >&2
        failed+=("$ex")
        continue
    fi

    echo
    echo "==> $ex"
    # `cargo test` builds the lib, the bin and the test targets, so a warning
    # anywhere in the example trips the crate's `[lints.rust] warnings = "deny"`
    # and fails here. No --locked: example Cargo.lock files are gitignored, so
    # there is no committed lockfile to hold cargo to.
    if (cd "$dir" && cargo test); then
        :
    else
        failed+=("$ex")
    fi

    record_peak
    # Drop this example's artifacts, keep the shared dependency build.
    (cd "$dir" && cargo clean -p "$ex" 2>/dev/null) || true
done

elapsed=$((SECONDS - started))

echo
echo "================================================================"
printf 'ran %d example(s) in %dm%02ds — peak target dir %.1f GB\n' \
    "${#targets[@]}" $((elapsed / 60)) $((elapsed % 60)) \
    "$(awk -v k="$peak_kb" 'BEGIN { printf "%.1f", k / 1048576 }')"

if [[ ${#failed[@]} -gt 0 ]]; then
    echo "FAILED (${#failed[@]}): ${failed[*]}"
    exit 1
fi
echo "all green"

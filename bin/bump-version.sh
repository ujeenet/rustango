#!/usr/bin/env bash
#
# Bump the workspace version everywhere it is written down.
#
#   bin/bump-version.sh 0.59.0
#
# A release bump touches twenty places across thirteen files — four
# manifests, a crate README, the docs manifest, and a `manage version` /
# MCP handshake transcript in each of four locales. Missing one is not
# visible on inspection: the page still reads correctly while naming a
# version that is not shipping. That is how the transcripts came to sit
# at 0.44.0 for thirteen releases, and how a 0.58.0 bump left the docs
# at 0.57 and failed CI on every open PR.
#
# `docs_versions` catches the omission. This removes the chance to make
# it.
#
# ## What it deliberately does NOT touch
#
# **CHANGELOG.md.** Its older sections name old versions on purpose —
# "Introduced in 0.51.1", "0.51.0 and 0.51.1 were yanked". A global
# search-and-replace would rewrite history into a lie. Same reason the
# doc edits below match labelled transcripts rather than every
# occurrence of the number: `MySQL 8.0.31` and `"openapi": "3.1.0"` are
# not this crate's version.

set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
gray()  { printf '\033[2m%s\033[0m\n'  "$*"; }

NEW="${1:-}"
if [ -z "$NEW" ]; then
  red "usage: bin/bump-version.sh <new-version>   e.g. 0.59.0"
  exit 1
fi
if ! printf '%s' "$NEW" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  red "version must be MAJOR.MINOR.PATCH — got '$NEW'"
  exit 1
fi

cd "$(git rev-parse --show-toplevel)"

# A dirty tree makes the bump's own diff unreviewable, which is the one
# thing that has to be read before it ships.
if [ -n "$(git status --porcelain)" ] && [ "${BUMP_ALLOW_DIRTY:-0}" != "1" ]; then
  red "working tree is dirty — commit or stash first"
  gray "  (override with BUMP_ALLOW_DIRTY=1 if you know why)"
  exit 1
fi

OLD=$(grep -m1 '^version = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
NEW_MM="${NEW%.*}"      # 0.59.0 -> 0.59, the docs series label

if [ "$OLD" = "$NEW" ]; then
  red "already at $NEW"
  exit 1
fi

gray "bumping $OLD -> $NEW"

# ---- manifests: the workspace version and every internal pin ----
MANIFESTS=(
  Cargo.toml
  crates/rustango/Cargo.toml
  crates/rustango-renamed-smoke/Cargo.toml
  crates/rustango-renamed-smoke/README.md
)
for f in "${MANIFESTS[@]}"; do
  perl -pi -e "s/\Q$OLD\E/$NEW/g" "$f"
done

# ---- docs manifest: major.minor, the URL the site publishes under ----
perl -pi -e "s/^version = \"[0-9]+\.[0-9]+\"/version = \"$NEW_MM\"/" docs/index.toml

# ---- transcripts, in every locale ----
#
# Matched by their label rather than by the bare number, so prose about
# older releases is left alone:
#
#   rustango 0.58.0                          <- `manage version`
#     version:        0.58.0                 <- `manage about`
#   "name": "rustango", "version": "0.58.0"  <- MCP handshake
for f in docs/manage.md docs/*/manage.md docs/mcp.md docs/*/mcp.md; do
  [ -f "$f" ] || continue
  perl -pi -e "s/(rustango )\Q$OLD\E/\${1}$NEW/g" "$f"
  perl -pi -e "s/(version:\s+)\Q$OLD\E/\${1}$NEW/g" "$f"
  perl -pi -e "s/(\"version\": \")\Q$OLD\E(\")/\${1}$NEW\${2}/g" "$f"
done

# ---- lockfiles ----
gray "refreshing Cargo.lock"
cargo metadata --no-deps --format-version 1 >/dev/null

# ---- verify, rather than claim ----
gray "verifying with the docs guards"
if ! cargo test -p rustango --no-default-features --features sqlite,tenancy \
      --test docs_versions 2>&1 | tail -8; then
  red "docs_versions still fails — a version reference was missed"
  gray "  the failure above names the file and line"
  exit 1
fi

echo
green "bumped $OLD -> $NEW"
gray "changed:"
git --no-pager diff --stat | sed 's/^/  /'
echo
gray "CHANGELOG.md is deliberately untouched — open a [$NEW] section by hand."

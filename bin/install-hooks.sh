#!/usr/bin/env bash
# Point this clone's git at the in-repo hooks directory.
#
# `core.hooksPath` is per-clone configuration — running this once
# after `git clone` opts your local repo into the rustango pre-commit
# / pre-push checks. Re-run after pulling new hooks.
#
# Optional companion tools the hooks pick up when present:
#   typos                — `cargo install typos-cli`
#   cargo-deny           — `cargo install cargo-deny`
#
# Usage:
#   bin/install-hooks.sh           install
#   bin/install-hooks.sh --uninstall   restore default .git/hooks

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "error: run from inside the rustango git working tree" >&2
  exit 1
}
cd "$repo_root"

case "${1:-}" in
  --uninstall)
    git config --unset core.hooksPath || true
    echo "uninstalled — git is back to .git/hooks"
    exit 0
    ;;
  ""|--install)
    if [ ! -d .githooks ]; then
      echo "error: .githooks/ not found at $repo_root" >&2
      exit 1
    fi
    chmod +x .githooks/pre-commit .githooks/pre-push 2>/dev/null || true
    git config core.hooksPath .githooks
    echo "installed — git will now use $repo_root/.githooks/"
    echo
    echo "hooks active:"
    for h in .githooks/*; do
      [ -x "$h" ] && echo "  $(basename "$h")"
    done
    echo
    echo "optional tools the hooks call when present:"
    for t in typos cargo-deny; do
      if command -v "$t" >/dev/null 2>&1; then
        echo "  $t — installed"
      else
        echo "  $t — NOT installed (cargo install ${t}-cli or ${t})"
      fi
    done
    ;;
  *)
    echo "usage: $0 [--install | --uninstall]" >&2
    exit 1
    ;;
esac

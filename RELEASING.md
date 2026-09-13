# Releasing rustango

Four crates ship to crates.io from this workspace, and they have to go
out in dependency order. Everything else here exists because it has
gone wrong at least once.

## 1. The bump PR — `release/vX.Y.Z` → `develop`

One version string lives in `[workspace.package]` in the root
`Cargo.toml`; the rest are pins that must move with it:

| file | what to change |
|---|---|
| `Cargo.toml` | `[workspace.package] version`, plus the three `[workspace.dependencies]` pins |
| `crates/rustango/Cargo.toml` | the `rustango-macros` pin |
| `crates/rustango-renamed-smoke/Cargo.toml` | the `orm = { package = "rustango", … }` pin |

`Cargo.lock` **is committed** — this workspace ships binaries, and the
example crates commit theirs too. Stage the lockfile changes; do not
assume they are ignored.

Then close the changelog: rename `## [Unreleased]` to
`## [X.Y.Z] — YYYY-MM-DD`, matching the form of the sections below it.

Sanity check before opening the PR:

```bash
cargo metadata --no-deps >/dev/null   # the manifests still resolve
cargo test -p rustango --no-default-features --features sqlite,tenancy --test docs_versions
```

That second one is the point of `docs_versions`: sample output in the
guides claims a version, and for thirteen releases it claimed the wrong
one. It fails until the docs agree with the manifests.

## 2. The release PR — `develop` → `main`

Merge it as a **merge commit** (`gh pr merge <n> --merge`), never a
squash — a squash would flatten develop's history onto main. Then tag
the merge commit and push the tag:

```bash
git tag -a vX.Y.Z <sha> -m "rustango X.Y.Z"
git push origin vX.Y.Z
```

### The stale-`main` trap

`origin/main` is a **develop-lineage** branch. Local `main` goes stale
and can sit hundreds of commits behind, or on the old hotfix lineage
that the `main-backup-pre-*` tag preserves.

**Always `git fetch` and base the promote on `origin/main`.** Merging
develop into a stale local `main` produces a large bogus conflict set;
merging into the real `origin/main` is small and clean.

Develop is canonical: **main must never hold content develop lacks.**
If main has diverged — it happened in 0.48.0, where a change had gone
straight to main and never through develop — stop and confirm with a
human before discarding anything. The main-only content may be a wanted
feature that simply got stranded, and it needs a follow-up issue to
reintroduce it on develop rather than a silent deletion.

## 3. Publish, in this order

```bash
cargo publish -p rustango-macros
cargo publish -p rustango
cargo publish -p rustango-orm-macros
cargo publish -p cargo-rustango
```

The order is not the one the manifests suggest at a glance:

- `rustango` normal-depends on `rustango-macros`.
- `rustango-orm-macros` normal-depends on `rustango-macros` **and
  dev-depends on `rustango`** — which is why it publishes *after*
  `rustango`, not before.
- `cargo-rustango` is an independent leaf. It does not depend on
  `rustango` at all, so it can go anywhere; last is as good a place as
  any.

`rustango-renamed-smoke` is `publish = false` and never ships.

`cargo publish` waits for the index to catch up before it returns, so
these can be run back to back. The token lives in
`~/.cargo/credentials.toml`.

### Publishing while the worktree is dirty

`cargo publish` refuses when the working directory has uncommitted
changes — which happens when someone else is working in the same tree.
Do **not** reach for `--allow-dirty` (it ships unreleased code) and do
not reset (it destroys their work). Publish from an isolated checkout
of the tag instead:

```bash
git worktree add --detach /tmp/rustango-publish-vX.Y.Z vX.Y.Z
cd /tmp/rustango-publish-vX.Y.Z
cargo publish -p <crate>            # path-deps resolve to the published versions
git worktree remove --force /tmp/rustango-publish-vX.Y.Z
```

## What maintains itself

Do not add these to a checklist; they are already handled:

- **Scaffolded projects** pin `rustango = "X.Y"` derived from
  `env!("CARGO_PKG_VERSION")` at compile time (`mm_version()` in
  `crates/cargo-rustango/src/main.rs`). Generated projects follow the
  release automatically.
- **`cargo install cargo-rustango`** is unpinned everywhere it appears
  in the docs, so it always resolves to the newest published version.
- **Doc transcripts** are guarded by
  `crates/rustango/tests/docs_versions.rs`, in all four locales.

## `gh pr checks` lags

It reports jobs as pending long after they have finished — `mysql_live`
and the CodeQL analysis are the usual offenders. For the real state use
`gh run view <run-id> --json jobs`, or watch
`gh pr view <n> --json mergeStateStatus` flip `BLOCKED` → `UNSTABLE` →
`CLEAN`. Do not burn a watcher loop trusting `gh pr checks` alone.

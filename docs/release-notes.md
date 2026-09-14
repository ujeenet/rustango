# Release notes

Every user-visible change ships with a changelog entry. This page is the route to them, because until now the docs site had none — a reader here could see version-labelled headings inside guides and nothing else, which is how a heading pinned to 0.42 came to read as "the docs stopped in 0.42" ([#1304](https://github.com/ujeenet/rustango/issues/1304)).

## Where the notes live

| What | Where | Best for |
|---|---|---|
| Full history, every version | [`CHANGELOG.md`](https://github.com/ujeenet/rustango/blob/main/CHANGELOG.md) | "when did this change, and what else moved with it" |
| One page per version | [GitHub Releases](https://github.com/ujeenet/rustango/releases) | "what is in the version I just upgraded to" |
| What is on the API right now | [docs.rs/rustango](https://docs.rs/rustango) | signatures, per-item docs |

The changelog is the source; a release page is the same content for a single version. Both are English only.

## How to read an entry

Entries are grouped under `Added`, `Changed`, `Fixed`, `Removed` and `Security`, newest version first, following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Two conventions worth knowing:

- **Entries say why, not just what.** An entry that changes behaviour explains what used to happen, because that is what tells you whether it affects you.
- **Issue numbers are load-bearing.** `(#1229)` links the discussion that produced the change, which usually has more detail than the entry has room for.

## Versioning

The project follows [SemVer](https://semver.org/) loosely, with one caveat that matters before 1.0: **nothing pre-1.0 carries a stability guarantee.** A minor version can change an API.

In practice the project avoids it, and a breaking change gets a `Changed` entry saying what to do. But the guarantee is not there yet, so pin a version you have tested rather than a range.

Two versions were yanked. `0.51.0` and `0.51.1` promised a migration reconcile that never fired against a real database; upgrade straight to `0.51.2` or later, which fixes both. See [Migrations](migrations.md) for what to do if you are on one of them.

## Upgrading

Bump the pin, run your tests, and read the `Changed` and `Removed` entries for every version you skipped — not just the one you land on. Most upgrades are a version bump and nothing else.

If a model changed, `cargo run -- makemigrations` shows you what moved before anything touches the database.

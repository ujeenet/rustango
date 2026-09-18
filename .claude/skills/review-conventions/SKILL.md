---
name: review-conventions
description: Conventions angle of a crew code review of rustango: ORM-over-raw-SQL, feature-gate hygiene, public API and semver on a 0.x patch release, naming rules, duplication, dead code and documentation drift. Invoked by review-aggregator with a run-id, or standalone for a conventions-only pass.
---

# Conventions review

You are the **conventions** reviewer on a crew of seven that review the same change from
different angles at the same time. You own one angle and nothing else. The
`review-aggregator` skill assigns your brief, fields your questions, and merges everyone's
findings; your peers are `review-correctness, review-security, review-tenancy, review-dialects, review-performance, review-tests`.

Your job is not to write the final review. Your job is to file precise, checkable findings
through the bus and to answer peers who are waiting on your angle.

## Wire up first

```bash
B=.claude/review/bus.sh
$B task <run-id> conventions
$B inbox <run-id> conventions
$B status <run-id> conventions working "starting on <n> files"
```

Read `.claude/review/PROTOCOL.md` once before you start. If invoked **without a run-id**,
work standalone against the target the user named and print findings in the same shape.

Never switch the shared checkout's branch. Work in
`git worktree add /tmp/rv-conventions --detach <sha>` and remove it when done.

## Your lane

Does this look like the rest of the codebase, and will the next person find what they need?
You own house rules, API shape, duplication and documentation truth. Not behaviour —
`clippy` and `rustfmt` already run in CI, so do not re-file what tooling catches.

## What to look for in this codebase

- **Reuse before writing.** The standing rule: grep for an existing helper first, and fix the
  framework if it nearly fits rather than hand-rolling beside it. The most common miss is a
  helper that exists but is private to its module — say so explicitly, because "the correct
  implementation exists and is walled off" is a more useful finding than "this is duplicated".

- **Always use the ORM.** Raw SQL needs a stated reason or a filed issue for the missing ORM
  support. A module that is entirely `raw_query_pool` / `raw_execute_pool` with zero
  `QuerySet::` is a standing debt worth naming once — not once per call site.

- **Duplicated *knowledge*, not just code.** One rule expressed in four places drifts: a
  ceiling as a named constant in one function and `.max(1).min(1000)` in two others, plus a
  number in prose. Note that `clippy::manual_clamp` is in `nursery` and this workspace
  enables `all` + `pedantic` only, so tooling will not catch that one.

- **Naming rules.** New ORM methods take the bare name — `_pool` is legacy and is not added
  to; `_on` is the executor variant. Emit bare-name wrappers for relation accessors.

- **Feature gates.** A `#[cfg(feature = "…")]` on an item whose callers assume a different
  gate is a build that fails for someone. Check the gate on a new module against what it
  imports, and check a new feature does not redundantly re-enable what a dependency already
  pulls in.

- **Public API on a 0.x patch release.** Adding a field to a public struct breaks struct
  literals downstream; adding an enum variant does not if the enum is `#[non_exhaustive]`.
  A `#[non_exhaustive]` **unit** variant cannot be matched downstream at all — an empty
  struct variant (`Foo {}`, matched `Foo { .. }`) is the shape that takes fields later.
  Flag anything that will need a breaking change to finish, while it is still free.

- **Deprecations.** A `#[deprecated]` with no stated removal version is a deprecation that
  never ends. Check the note names the replacement.

- **Documentation drift — verify against code, never against another doc.** Two pages
  agreeing with each other is not evidence. Watch for:
  - An outer `///` on a `pub mod` declaration: it concatenates with the module's own `//!`
    and resolves intra-doc links in the *parent's* scope, silently breaking every relative
    link inside. Invisible locally when the crate already emits hundreds of rustdoc warnings.
  - ` ```ignore ` examples — nothing compiles them, so they rot. A published example that
    calls a PostgreSQL-only constructor while presenting itself as portable is the classic.
  - A behaviour change with an unchanged signature (a new cap, a widened delete). Rustdoc is
    where a caller looks before CHANGELOG; both should say it.
  - Published lists the code owns: a route table, a set of tracing targets, a feature matrix.
    `docs_inventories` guards some of these — check whether a new one needs a guard.

  The `docs-truth` skill has the full method and the traps; load it for a docs-heavy diff.

- **CHANGELOG and UPGRADING.** A change that is silent in both directions — no compile error,
  no runtime error, just different behaviour — is the one that *must* be written down. Check
  every user-visible change in the diff has an entry, and that a breaking one has an
  UPGRADING row findable from the symptom rather than from the API name.

- **Dead code and leftovers.** Unused code after a change, a commented-out block, a stale
  comment above a line that no longer does what it says.

## Stay in your lane

- Behaviour -> `correctness`
- Authz, secrets -> `security`
- Pool/tenant routing -> `tenancy`
- Backend divergence -> `dialects`
- Cost -> `performance`
- Whether a test can fail -> `tests`

```bash
$B send <run-id> conventions <peer> handoff "<subject>" --body "<file:line + what you saw>"
```

## Filing findings

```bash
echo '{"severity":"medium","confidence":"confirmed","category":"api-shape",
  "file":"crates/rustango/src/media/router.rs","line":117,
  "title":"non_exhaustive unit variant cannot be matched downstream",
  "detail":"What is wrong and why it matters.",
  "failure_scenario":"A downstream policy matching this variant fails to compile (E0603).",
  "fix":"The specific change you would make.",
  "evidence":["crates/rustango/src/media/router.rs:117"]}' | $B finding <run-id> conventions
```

Rules:
- One finding per defect. Most of yours will be `low`/`medium`; that is correct for this
  angle. Do not inflate to be heard.
- **Say when a convention should not be followed here.** "This duplication is fine and a
  helper would cost more than it saves" is a real finding and saves the author work.
- Do not file what `clippy` or `rustfmt` already fails on in CI.
- Corroborate rather than duplicate; `dispute` with a file:line reason.
- Nothing to report is a valid result.

## Finish

```bash
$B inbox <run-id> conventions
$B status <run-id> conventions done "<n> findings: <headline>"
```

Then reply with 3-5 lines: what you covered, what you skipped, your highest-severity finding,
and any handoff still outstanding.

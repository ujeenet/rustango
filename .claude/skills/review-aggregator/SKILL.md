---
name: review-aggregator
description: Run a multi-angle crew code review of rustango — scope the change, assign a focused brief to each specialist reviewer (correctness, security, tenancy, dialects, performance, tests, conventions), dispatch them in parallel, serve their mail on the .claude review bus, then dedupe, adjudicate and rank everything into one report. Use for "crew review", "multi-angle review", "review this from all angles", PR reviews, or any review where one pass is not enough.
---

# Review aggregator

You own the run. You do **not** review code yourself — seven specialists do that, each
through one lens, in parallel, and you turn their raw findings into a single report the
author can act on. Your value is scoping, dispatch, and adjudication: one deduped, ranked,
verified list instead of seven overlapping ones.

Read `.claude/review/PROTOCOL.md` first. It is the contract you and the reviewers share.

```bash
B=.claude/review/bus.sh
```

## 1. Scope the target

Work out what is under review before you spend seven agents on it:

| target | how to resolve it |
|---|---|
| nothing said | uncommitted diff: `git status --short`, `git diff`, `git diff --cached` |
| "this PR" / a number | `gh pr view <n> --json headRefName,baseRefName,title,body`, then diff the head against the base |
| a branch | `git diff $(git merge-base develop <branch>)..<branch>` |
| a stacked PR | diff against **its own base branch**, not `develop` — a stack's upper PRs otherwise show every commit below them |
| paths | those files as they stand |

Then read the diff yourself — `--stat` plus the actual hunks. You need to know what changed
to write briefs worth following; a generic brief produces a generic review. Note the base
ref, the feature flags touched, and anything that looks risky.

**Diff the base against the tip, not commit by commit.** Behaviour that four individually
reasonable commits broke jointly is exactly what per-commit reading misses — and in a
stacked PR, a branch left on a stale base silently drops work from the branch below it.

## 2. Pick the roster

Default to all seven. Drop an angle only when the diff genuinely cannot contain its
findings, and say which you dropped and why:

- `dialects` — no SQL, no migrations, no `Dialect` dispatch, no schema
- `tenancy` — nothing touching `Org`, `TenantPools`, per-tenant pools, or the registry
- `performance` — a few lines with no loops, queries, or per-row work

Keep `correctness`, `security` and `conventions` on almost every run. For a large diff,
scope each reviewer to the files that matter for its angle rather than trimming the roster —
seven narrow reviews beat four broad ones.

```bash
$B init <run-id> --target "PR 1553: bound the media contents listing" --base release/v0.57.7 \
  --roster correctness,security,dialects,performance,tests,conventions --effort medium
```

Use a run-id that reads well later: `pr-1553`, `media-stack`, `wip-2026-09-17`.

## 3. Write one brief per reviewer

This is the step that decides the quality of the whole run. A brief is not the angle's name
back at it — the skill already knows its own lane. A brief says *what in this diff* deserves
that lens, and what you already know.

```bash
$B assign <run-id> dialects \
  --focus "New paged listing builds an IN(...) list by hand and appends LIMIT/OFFSET. \
Check bind order against text order, and what a negative limit does on each backend." \
  --paths "crates/rustango/src/media/mod.rs" \
  --notes "PostgreSQL is numbered (\$n), SQLite and MySQL are positional (?). The live \
suite is PG-only, so a SQLite/MySQL bug here would not show up in CI."
```

Good briefs name files, name the suspicion, and pass on context the reviewer cannot see
(a related past bug, an author note, a decision already made). If you noticed something
while reading the diff, hand it to the owning angle as a starting point — do not sit on it,
and do not file it yourself.

**Tell reviewers what is already known.** A list of issues already found and being fixed
is worth as much as the brief itself: it stops seven agents re-deriving the same thing and
frees them for new ground.

## 4. Dispatch in parallel

Invoke this skill from a **main session**, not from inside another subagent — dispatch needs
the Agent tool, and a subagent may not be able to spawn further agents. One session runs the
whole crew; nobody needs to open a second window.

Spawn every reviewer in **one** message with multiple Agent calls so they run concurrently,
in the background — seven separate context windows, each reading the diff through one lens.
Each gets the same shape of prompt:

> Invoke the `review-<angle>` skill for run `<run-id>` in
> `/Users/ievgeniisvyryd/projects/rustango`. Your brief is on the bus:
> `.claude/review/bus.sh task <run-id> <angle>`. Follow that skill exactly — set your status,
> file findings through the bus, answer your inbox, and report `done` when finished.
> Do not edit any file outside `.claude/review/runs/<run-id>/`.

Reviewers are `general-purpose` agents (they need Bash for the bus, Read/Grep for the code).
They do not spawn agents of their own.

Three rules that keep a crew run from turning into a mess:
- **Reviewers never write code.** They file findings; fixes come later, from the author or
  from a separate pass.
- **Reviewers write only their own bus files.** The single-writer rule in the protocol is
  what makes parallel dispatch safe.
- **Reviewers never switch the shared checkout's branch.** Concurrent sessions share one
  worktree here. Tell them to use `git worktree add /tmp/rv-<angle> --detach <sha>` and
  remove it when done.

### Disk

Each worktree a reviewer builds carries its own `target/`, and every distinct feature-set
permutation produces a full set of rlibs. Seven concurrent builds of this workspace is tens
of gigabytes. Check `df -h /` before dispatch, and prefer pointing reviewers at a shared
`CARGO_TARGET_DIR` when space is tight. A run that fills the disk takes the whole session
down with it, including your ability to clean up.

## 5. Serve the bus while they work

You are the switchboard. Between agent-completion notifications:

```bash
$B board <run-id>        # state, finding count, unread mail per reviewer
$B inbox <run-id> aggregator
```

- Answer `question` mail — usually scope ("is the deprecated path in scope?" -> decide, reply).
- Route `handoff` mail that arrives for you to the angle that owns it, or file it as a note
  for the report if nobody owns it.
- Unblock `blocker` mail immediately: a blocked reviewer is doing nothing.
- If two reviewers are circling the same code, tell one of them to defer and `corroborate`.

Do not poll with `sleep`. You are notified as each background agent finishes; if you want a
live watch, use the `Monitor` tool with an until-loop on `$B board <run-id>`.

Before you move on, check the board for a reviewer that reported `done` with zero findings
*and* an unread inbox — it probably skipped its mail. Re-send anything that still needs an
answer.

## 6. Merge, adjudicate, verify

```bash
$B collect <run-id>   | jq -c '.[]|{id,reviewer,severity,confidence,file,line,title}'
$B clusters <run-id>                 # grouped by file — this is where the duplicates are
```

Then, in order:

1. **Dedupe.** Two findings are the same defect when they predict the same failure, even
   when the files or words differ. Keep the clearest one, list the other ids as
   corroborating, and credit both angles. Never ship the same defect twice.
2. **Upgrade on agreement.** Independent corroboration from a second angle raises confidence
   — and a dialect finding that `correctness` also reached usually deserves a higher severity
   than either filed alone.
3. **Resolve disputes.** Read the disputed code yourself and rule. Say who was right in one
   line; do not pass an unresolved disagreement to the author.
4. **Verify everything `critical` or `high`.** Open the file, read the path end to end, and
   confirm the `failure_scenario` actually follows. A wrong critical costs the crew its
   credibility. Downgrade what does not hold up, drop what is plainly wrong, and mark the
   rest `CONFIRMED`. For a big batch, spawn verifier agents — one per finding, in parallel —
   with the finding JSON and instructions to confirm or refute from the code alone.
5. **Cut the noise.** Drop `speculative` findings nobody corroborated and that you cannot
   confirm, style nits already handled by `clippy`/`rustfmt`, and pre-existing issues the
   diff does not touch (keep those as a short "outside this change" list).
6. **Normalize severity across angles.** Seven reviewers drift. Re-rank the survivors against
   the protocol's rubric as one list, so `high` means the same thing everywhere.

**A reviewer can be confidently wrong.** Verify the claim, not the confidence. In practice
the most common bad finding here is a measurement taken on the wrong backend or on a
long-lived database that carries state from an older schema — check what was measured
against, not just what was concluded.

## 7. Report

Write `.claude/review/runs/<run-id>/report.md` and close the run:

```markdown
# Review: <target>
<n> findings from <k> angles · base <ref> · run <run-id>

## Verdict
One paragraph: is this safe to merge, and what is the one thing to fix first.

## Findings
### 1. [CRITICAL] <title> — `crates/rustango/src/media/mod.rs:942`
**Angle:** dialects (corroborated by correctness)
**Why it matters:** <impact>
**Failure:** <concrete scenario>
**Fix:** <specific change>

## Agreed non-issues
Disputes resolved in the code's favour, and what was checked.

## Coverage
Which angles ran, what each skipped, which backends were actually exercised, and any angle
left off the roster.
```

```bash
$B close <run-id> reported
```

In chat, give the verdict, the counts by severity, and the top three findings with file:line
— not the whole report; the author can open it. If the host UI supports it, also report the
verified findings through the `ReportFindings` tool, most severe first.

State the crew's limits honestly: which angles ran, which files nobody opened, **which
backends were actually run against**, and anything a reviewer flagged as unverified. A
review whose gaps are stated is more useful than one that implies it saw everything.

## Degraded runs

- **A reviewer never reports `done`** — take what is in its findings file, note it as
  incomplete in Coverage, and move on. Never invent a reviewer's result or predict what a
  still-running agent will say.
- **A reviewer is `blocked`** — answer it if you can, otherwise re-assign a narrower brief.
- **Everyone files nothing** — that is a real and reportable outcome. Say what was examined.
- **Findings contradict the diff** — trust the code, not the finding.
- **The disk fills mid-run** — every tool call fails, including cleanup. Ask the user to free
  space; do not keep retrying.

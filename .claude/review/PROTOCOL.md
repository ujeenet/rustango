# Review bus protocol

Every reviewer skill (`review-<angle>`) and `review-aggregator` coordinate **only** through
files under `.claude/review/runs/<run-id>/`. No reviewer talks to another directly; no
reviewer writes its findings into chat as the deliverable. Use `.claude/review/bus.sh`
for all reads and writes — it validates the schemas and assigns finding IDs.

## Roles

- **Aggregator** — owns the run. Creates it, scopes it, assigns one focus per reviewer,
  spawns the reviewers, answers their mail, then clusters/dedupes/adjudicates the findings
  into one report. Never reviews code itself.
- **Reviewer** — one angle only. Reads its task, reviews the target through that lens,
  files findings, answers mail from peers, reports `done`.

## Run layout

```
.claude/review/runs/<run-id>/
  run.json              scope, target, base ref, effort, roster, state
  tasks/<r>.json        the assignment the aggregator wrote for reviewer <r>
  findings/<r>.jsonl    reviewer <r>'s findings, one JSON object per line (only <r> writes)
  status/<r>.json       assigned | working | blocked | done, plus a short note
  mail/<r>/*.json       unread mail for <r>;  mail/<r>/_read/ is what it has consumed
  report.md             the aggregator's final report
```

Single-writer rule: a reviewer writes only `findings/<self>.jsonl`, `status/<self>.json`,
and mail addressed to others. That is what makes parallel reviewers safe — nobody ever
edits a file another agent is appending to.

## Commands

```bash
B=.claude/review/bus.sh

# aggregator
$B init <run> --target "<what is under review>" [--base <git-ref>] [--roster a,b,c] [--effort low|medium|high]
$B assign <run> <reviewer> --focus "<one angle-specific brief>" [--paths a,b] [--notes "<text>"]
$B board <run>                  # state + finding count + unread mail per reviewer
$B collect <run> [reviewer]     # all findings, severity-sorted, as one JSON array
$B clusters <run>               # findings grouped by file — shows cross-reviewer overlap
$B close <run> [state]

# reviewer
$B task <run> <self>            # your brief + the shared run context
$B status <run> <self> working "<note>"
echo '<json>' | $B finding <run> <self>     # object or array; prints assigned IDs
$B inbox <run> <self>           # unread mail, marks it read ( --peek to leave unread )
$B send <run> <self> <to|all> <kind> "<subject>" --body "<text>" [--refs id1,id2]
$B status <run> <self> done "<n findings, one line of headline>"
```

## Finding schema

Required: `severity`, `title`, `file`, `detail`. `failure_scenario` is required when
severity is `critical` or `high`. `id`, `reviewer`, `reported` are filled in for you.

```json
{
  "severity": "critical|high|medium|low",
  "confidence": "confirmed|likely|speculative",
  "category": "kebab-case-slug",
  "file": "crates/rustango/src/media/mod.rs",
  "line": 942,
  "title": "<= 60 chars, the claim alone",
  "detail": "One or two sentences: what is wrong and why it matters.",
  "failure_scenario": "Concrete inputs/state -> the wrong outcome.",
  "fix": "The specific change you would make.",
  "evidence": ["crates/rustango/src/media/mod.rs:942", "crates/rustango/src/sql/dialect.rs:104"],
  "cross_ref": ["tenancy-003"]
}
```

Severity rubric — the same words must mean the same thing across all seven angles:

| severity | meaning |
|---|---|
| `critical` | Data loss/corruption, cross-tenant leakage, auth bypass, money moved wrongly, or prod outage. Reachable on a normal path. |
| `high` | Wrong behaviour a user will hit, a security hole needing an unusual precondition, or a severe perf regression. |
| `medium` | Real defect on an edge path, missing error handling, meaningful perf or test gap. |
| `low` | Convention, naming, dead code, readability. No behaviour change. |

Confidence — `confirmed` = you read the code path end to end and it is wrong;
`likely` = strong reading, one assumption unverified; `speculative` = worth a look, say why.

## Mail

Kinds: `question`, `answer`, `handoff`, `corroborate`, `dispute`, `fyi`, `blocker`.

- `handoff` — you spotted something outside your lane. Do not file it; hand it to the angle
  that owns it. If no reviewer owns it, send it to `aggregator`.
- `corroborate` — you independently see a peer's finding. Send it; do **not** file a
  duplicate. The aggregator upgrades confidence when two angles agree.
- `dispute` — you believe a peer's finding is wrong or already handled elsewhere. Say why
  and cite a file:line. Disputes carry weight in adjudication.
- `blocker` — you cannot proceed (missing scope, unreadable target). Send to `aggregator`
  and set your status to `blocked`.

Check your inbox at the start, once mid-review, and before you report `done`. Answer every
`question` addressed to you — a peer is waiting on it. Keep bodies under ~10 lines.

## Waiting

Nobody polls with `sleep`. The aggregator spawns reviewers as background agents and is
notified as each finishes; between notifications it reads `board` and its own inbox.
To watch the bus continuously, use the `Monitor` tool with an until-loop on
`bus.sh board <run>` — never a foreground sleep loop.

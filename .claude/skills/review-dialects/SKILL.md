---
name: review-dialects
description: Tri-dialect angle of a crew code review of rustango: behaviour that differs between PostgreSQL, MySQL and SQLite — placeholder/bind order, LIMIT and ordering semantics, DDL the migration snapshot path drops, and claims proven on one backend but made about three. Invoked by review-aggregator with a run-id, or standalone for a dialect-only pass.
---

# Tri-dialect review

You are the **dialects** reviewer on a crew of seven that review the same change from
different angles at the same time. You own one angle and nothing else. The
`review-aggregator` skill assigns your brief, fields your questions, and merges everyone's
findings; your peers are `review-correctness, review-security, review-tenancy, review-performance, review-tests, review-conventions`.

Your job is not to write the final review. Your job is to file precise, checkable findings
through the bus and to answer peers who are waiting on your angle.

## Wire up first

```bash
B=.claude/review/bus.sh
$B task <run-id> dialects
$B inbox <run-id> dialects
$B status <run-id> dialects working "starting on <n> files"
```

Read `.claude/review/PROTOCOL.md` once before you start. If invoked **without a run-id**,
work standalone against the target the user named and print findings in the same shape.

Never switch the shared checkout's branch — other sessions are using it. Work in
`git worktree add /tmp/rv-dialects --detach <sha>` and remove it when done.

## Your lane

rustango claims tri-dialect support: one body of code, one body of test, three backends
(PostgreSQL, MySQL, SQLite). You are the reviewer who checks that the claim holds for this
change. **Your defining question is: which backends was this actually run on, and what
would the other two do?**

This is the highest-yield angle in this repo, for one structural reason: the live test
suites are overwhelmingly PostgreSQL- or SQLite-only, so a bug that is correct on the tested
backend and wrong on the others ships green. Issue #1461 is the standing epic.

## What to look for

- **Placeholder vs bind order — the trap that has shipped twice.**
  `Dialect::placeholder(n)` **ignores `n`** and returns positional `?` on SQLite and MySQL;
  only PostgreSQL emits `$n`. So on two of three backends the bind vector must follow the
  order the placeholders appear in the **SQL text**, not the order the numbers suggest.

  ```
  UPDATE t SET ts = {p1} WHERE id IN ({p2}, {p3})   binds [id1, id2, now]
  ```
  On PostgreSQL that is correct. On SQLite and MySQL the first id lands in `ts`. Same bind
  count, no error, silently wrong rows — and a PG-only suite stays green throughout.

  The safe shape derives the index from the bind vector as it pushes
  (`sql::writers::Sql::push_param` does this); anything computing `len() + k` by hand
  deserves a line-by-line check. The other half of the same trap: reusing a number (`$1`
  twice) is one bind on PostgreSQL and two `?` elsewhere.

- **`LIMIT` and `OFFSET` semantics diverge.** `LIMIT -1` means *no limit* on SQLite and is
  an error on PostgreSQL; `LIMIT 0` returns nothing rather than everything. An unclamped
  caller-supplied limit is therefore an unbounded scan on one backend and a 500 on another.
  Check that every paged query clamps before binding.

- **Ordering.** `ORDER BY <non-unique>` with `LIMIT/OFFSET` is not a total order, and the
  planners disagree about tied rows. PostgreSQL's top-N sort orders ties differently per
  (limit, offset); SQLite tends to fall back to rowid order and *masks* the bug. A guard
  written on SQLite for this class cannot fail.

- **MySQL is the strict one.** `ONLY_FULL_GROUP_BY` and `STRICT_TRANS_TABLES` are on by
  default in MySQL 8. TEXT-in-index is rejected where PostgreSQL and SQLite accept it. A
  `DELETE` cannot re-open its own target table in a subquery (a *different* table is fine).
  `ON DELETE SET NULL` on a NOT NULL column fails the DDL with `ERROR 1830` rather than at
  delete time.

- **The migration snapshot path.** System migrations and `testkit::migrate_framework` render
  from a `SchemaSnapshot`, **not** from a `ModelSchema`. Anything a `FieldSchema` carries
  that `RelationSnapshot`/`FieldSnapshot` has no field for is silently dropped before it
  reaches any database — this has happened to `generated_as`, `db_comment` and
  `fk_on_delete`. A guard that renders from a `ModelSchema` cannot catch it.

- **`testkit::emit_tables` does not create composite-unique indexes.** So `fresh_table` on a
  `unique_together` model builds a schema that cannot enforce the uniqueness the test is
  about — and the symptom is silent (an idempotent insert quietly duplicating), not an
  error. Framework-managed tables should go through `migrate_framework` instead.

- **Hardcoded PostgreSQL.** Route through `crate::sql::Pool` and the dialect emitters rather
  than `PgPool`. Litmus: `cargo build --no-default-features --features sqlite,tenancy`.
  Schema-mode tenancy is the one sanctioned PG-only exception.

- **sqlx never resets session state.** Pool release only pings, so any session-level `SET`
  leaks to the next borrower.

## How to actually check

Reading is not enough for this angle. Stand the backends up:

```bash
docker compose up -d postgres            # 5432, rustango/rustango
docker run -d --name my3407 -p 3407:3306 \
  -e MYSQL_ROOT_PASSWORD=rustango -e MYSQL_DATABASE=rustango_test \
  -e MYSQL_USER=rustango -e MYSQL_PASSWORD=rustango mysql:8.0   # 3406 is often taken
```

Prefer driving a real `MediaManager`/`QuerySet` on each pool over pasting raw SQL into a
client — a bind-order bug lives in how Rust orders the vector, not in the SQL text, so a
hand-run statement probe cannot see it. That distinction has already let one regression
through.

Remove containers and worktrees when you finish.

## Stay in your lane

- A logic bug that is wrong on all three -> `correctness`
- Injection, authz -> `security`
- Which pool / which tenant -> `tenancy`
- Cost rather than correctness -> `performance`
- "No test covers this backend" -> `tests` (but say *which* backend and why it matters)
- ORM-vs-raw-SQL as a convention -> `conventions`

```bash
$B send <run-id> dialects <peer> handoff "<subject>" --body "<file:line + what you saw>"
```

## Filing findings

```bash
echo '{"severity":"critical","confidence":"confirmed","category":"dialect",
  "file":"crates/rustango/src/media/mod.rs","line":962,
  "title":"Bind order disagrees with text order — wrong row updated on SQLite and MySQL",
  "detail":"What is wrong and why it matters.",
  "failure_scenario":"Concrete inputs -> wrong outcome, and on which backends.",
  "fix":"The specific change you would make.",
  "evidence":["crates/rustango/src/media/mod.rs:962"]}' | $B finding <run-id> dialects
```

**Always name the backends** in `failure_scenario` — which are affected, which are not, and
which you executed against. "Broken on SQLite and MySQL, correct on PostgreSQL, measured on
all three" is the shape that makes a finding actionable. A dialect finding without that is
half a finding.

Rules:
- One finding per defect. `critical`/`high` require a real `failure_scenario`.
- Corroborate rather than duplicate; `dispute` with a file:line reason.
- Nothing to report is a valid result — and for this angle, "I ran it on all three and they
  agree" is a genuinely valuable thing to say. Say what you ran.

## Finish

```bash
$B inbox <run-id> dialects
$B status <run-id> dialects done "<n> findings: <headline>"
```

Then reply with 3-5 lines: which backends you actually exercised, what you covered, your
highest-severity finding, and anything you could only read rather than run.

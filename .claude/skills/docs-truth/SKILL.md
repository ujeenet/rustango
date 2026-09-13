---
name: docs-truth
description: Verify rustango documentation against the code that implements it, and make a page testable. Use when writing or editing anything under docs/, when a doc claim needs checking, when auditing a page for drift, when paying down the untested-docs backlog in crates/rustango/tests/docs_contract.rs, or when a user reports that a documented API doesn't work. Covers the verification method, the seven traps that have produced wrong findings, how to write the guard, and the severity taxonomy.
---

# Verifying rustango docs against the code

Documentation here is checked by execution, not by reading. Reading is exactly the check a confidently-worded wrong sentence passes: `fetch_pool()` sat in the README's headline example for two releases after the rename, and `.execute(&pool)` appears eleven times in `orm.md` and has never existed. Both read fine.

A September 2026 audit found **107 disagreements** between the docs and the code. This skill is the method that found them — plus the seven ways it got things wrong, which turned out to be the more useful half. Of 19 findings escalated as code defects, two were withdrawn and one was corrected after filing, every one caught by a second reader rather than the original pass.

## The rule

**Never edit prose to match a defect.** When a doc describes the correct behaviour and the code doesn't do it, you have found a bug, not a typo. Editing the sentence documents the hole and closes the only signal that it exists.

Decide which side moves, and when it's the code, file an issue rather than silently "fixing" the doc. Four of the audit's worst findings were security controls the docs promised and the code didn't provide — the docs were right every time.

## Before you write any example

Run the correlation scanner:

```bash
python3 bin/docs-graph.py --summary
```

It maps each page to its backing tests, the symbols it names, whether those symbols resolve, and which of them are Postgres-gated. Use `--unresolved-only` to see just the pages with unresolved symbol mentions.

It is **advisory, not a gate** — the unresolved column includes SQL keywords and example type names, so a nonzero count is not automatically a bug. It tells you where to look.

The gate is `cargo test -p rustango --test docs_contract`.

## Verifying a claim

For every concrete statement — an API name, a default, a flag, a config key, a status code, a feature gate:

1. **Find the definition.** `rg -n 'fn <name>' crates/` — not the call site, the definition. A doc naming a method that doesn't exist is the single most common finding.
2. **Check the signature**, not just the name. Arity and parameter types are where `.filter("x", Op::Eq, v)` went wrong: `filter` is two-arg, `filter_op` is three, and `exclude_op` doesn't exist.
3. **Check the feature gate.** `#[cfg(feature = "postgres")]` within ~8 lines above a definition means the method vanishes on a sqlite build. A doc showing it as portable is a finding.
4. **Check the default in code**, never in the doc's own prose. "Defaults to X" claims were wrong for the page-size cap (100, documented 1000), the tenancy pool (16, documented 4), and the retry backoff (2s, documented 1s).
5. **Run it if the behaviour is reachable.** Say which you did. "Reproduced by execution" and "verified by inspection" are different claims and issues should distinguish them.

## Seven traps that have already produced wrong findings

Every one was caught by a second reader, not by the pass that made the claim. Assume they'll recur, and assume you won't spot your own.

### Family generalisation

A claim of the form *"every X does Y"*, verified against one member of X.

`api-conventions.md` was flagged for teaching the pool/executor convention backwards. True. But the proposed correction — "the bare name is the multi-backend path" — was itself wrong, because the family isn't uniform:

- **Reads** (`fetch`, `count`, `first`, `find`, `exists`) take `&Pool`, ungated. The rule holds.
- **Writes** (`save`, `insert`, `delete`) take `&sqlx::PgPool` and are `#[cfg(feature = "postgres")]`. The rule inverts. Their multi-backend siblings are `save_pool` / `insert_pool` / `delete_pool`, and on a sqlite build `save` does not exist at all. Tracked in **#1293**.

The same pass claimed "exactly six types implement `tower::Layer`" from one grep; the real number is higher. The substantive point held, the enumeration didn't.

**So:** check two or three members of any family before writing a rule about it, and prefer naming the members over counting them — a count rots and a list can be re-grepped.

**It applies to prose too, which is easier to miss.** Four wrong sentences about one subsystem are not necessarily four instances of *one* wrong sentence, and the fix that clears three can leave the fourth standing.

`jobs.md` carries two different errors about retries. Three sites state the backoff sequence wrongly. A fourth, `jobs.md:79`, calls `MAX_ATTEMPTS` "the retry ceiling" — but the guard is `next_attempt >= max_attempts` with `attempt` starting at 0, so the default 5 gives one initial run plus **four** retries. Correcting the shift does nothing for that line. A finding written as "four sites, one error" would have shipped a fix that left it wrong, and someone setting `MAX_ATTEMPTS = 3` expecting three retries would still get two.

Same file, same subsystem, same feature — and still two findings. Group sites by *the claim they make*, not by where they live.

### Self-implicating is not the same as verified

Evidence offered against the speaker's own interest reads as credible, and credibility is not correctness. A confession invites agreement rather than checking — nobody wants to be the one doubting it.

`crates/rustango/README.md` was reported as a byte-identical copy of the root README whose links break on crates.io. The supporting evidence was: *"I added one line to the root README myself and the two diverged within hours — root `184a06c0`, crate `88b93484`."* That reads as unusually strong because the reporter is implicating their own change.

It was an artifact. The file is a **symlink** (`git ls-tree` mode `120000`), created deliberately to fix a blank crates.io README. `git show <rev>:<symlink>` prints the *link target*, so the comparison was an 18KB file against the 15-byte string `../../README.md`. A symlink cannot diverge from what it points at. A `shasum` run earlier had correctly reported them identical — and was overridden because the later tool seemed more authoritative.

Two sessions then endorsed it, because the second reader praised the evidence instead of checking it.

**So:** check the file mode before reasoning about duplication — and more generally, when two tools disagree, ask which one is answering the question you asked, not which one ran later. Treat a self-implicating claim like any other: it earns the same check, not less.

### Numbers you narrowed before counting

Three counts in one audit were reported wrong in the same way: "52 relative links" (55), "two COOKBOOK links" (four), and the SHA above. Each came from re-reading the output of a query built for a *different* question.

The four COOKBOOK links are the clearest case: a grep narrowed to ``](crates/rustango/examples/`` found the two obvious ones and missed one inside an ORM-guide line and one in a summary block.

**So:** when you publish a count, re-run the query scoped to the claim you are actually making. A number is a claim, and it will be quoted back with more confidence than you meant.

### Two reachable paths

A default that differs between the builder and the `Cli` path. Check both, and note that **the `Cli` path is usually the fail-closed one and the one users are actually on**.

Cookie `Secure` was filed as a security defect because `resolve_secure_cookies` falls back to the environment tier rather than the request scheme. True — but `manage.rs:443` passes `secure_cookies.unwrap_or(true)`, so on the documented path `Secure` is on for every tier. The practical default is *stricter* than the doc promises. Real finding, wrong severity.

### The doc written from a comment

A source comment is not the code. When a doc and an inline comment agree and both disagree with the code, the doc was almost certainly written from the comment — and fixing only the doc leaves the next writer the same wrong source.

The retry backoff is the canonical case, because the whole chain is visible in one file:

- `jobs/mod.rs:405` — `// Re-enqueue after backoff (1s, 2s, 4s, 8s, ...)`, sitting **directly above the calculation it describes**
- `jobs/mod.rs:50` — the module doc, written from that comment
- `docs/jobs.md:242` — the guide, written from the module doc

The code is `1000ms << next_attempt` with `attempt` starting at 0, so `next = 1` and the first retry waits **2s**, not 1s. Both backends compute identically, so `mod.rs` and `pg.rs` agree with each other and disagree with all three pieces of prose. Fix one and the others regenerate it.

Note the direction: the error propagated *outward* from a comment that was wrong about the line beneath it. The guide is the last victim, not the source — so fixing the guide is the one edit that changes nothing.

**So:** when a doc claim is wrong, grep the source comments for the same claim before you edit, and follow it to the innermost one. If they match, fix the whole chain in one commit and say so — otherwise the doc rots back from a source nobody looked at.

### A wrong comment in a copyable snippet ships

Wrong prose misleads a reader who can still go and check the code. A wrong comment inside a snippet people paste lands **in their codebase**, and from then on it is their bug, in their repo, with nothing pointing back here. Different blast radius, so rank it higher.

`jobs.md:226` ships this inside a snippet readers paste into their own `main`:

```rust
tokio::signal::ctrl_c().await?;   // block until Ctrl-C / SIGTERM
```

`ctrl_c()` is SIGINT only. The comment is a factual error about tokio's API, and there is no SIGTERM handling anywhere in the crate — so the snippet teaches a reader to believe their worker drains on deploy when it does not.

**So:** treat comments inside examples as code, not commentary. They are the part most likely to be copied verbatim and least likely to be checked.

### "The help says X" is false under most builds

Help text, error strings and CLI output are often inside `#[cfg(...)]` blocks. A claim about what a command prints is incomplete until it names the feature set that renders the line.

The `create-user-key` help string lives in a `#[cfg(feature = "mcp")]` block; a test pinning it failed as an unconditional assertion because without that feature the line simply isn't emitted.

**So:** when a finding quotes CLI output, record the feature set it was observed under. `cargo run -- --help` on a default build and on `--no-default-features --features sqlite,tenancy` are different documents.

## Writing the guard

These rules are about the tests, not the docs. They cost more to learn than to read.

### A guard that needs exceptions is measuring the wrong thing

If making a check pass requires a growing allowlist, the check has stopped asserting the property and started asserting its own workarounds. Delete it.

A test asserting "every verb in `--help` has a dispatcher arm" reported 27 missing verbs — including `cargo`, `tenant`, `a` and `name` — because the help block mixes verb entries, wrapped prose and `cargo run --` examples at one indent level. No amount of parsing fixes that; the rendered text isn't structured data. The honest fix is making the help block *data* the dispatcher and the printer share, so the two cannot disagree. That's a refactor, not a test.

**So:** when a guard needs its third exception, stop and ask what it is really measuring. Record the deletion and the reason in the file where it lived, or the next person derives it, fights it, and deletes it again.

### A gated test file needs one ungated test that reports the gate

`cargo test --test tenancy_help_matches_behaviour` printed `ok. 0 passed`. Every test in the file was behind `#[cfg(feature = ...)]`, so on a build without those features nothing ran and the result still read green — on the file you run precisely when you are editing the strings it guards.

**The obvious fix is wrong here.** A `#[cfg(not(...))] #[test] fn { panic!() }` stub looks right until you read the feature graph: `default = ["postgres", "batteries"]` and `batteries` includes `manage` but **not** `tenancy`. So a plain `cargo test -p rustango` — the most common command in the repo — has one and not the other, and the stub would fail on a configuration that is entirely fine. A guard that cries wolf on the default invocation gets deleted, and rightly.

Nor would CI have caught it: `feature_combos` runs `cargo check`, not `cargo test`. The breakage lands on people, not on pipelines.

What works is an always-compiled test that *reports* what ran:

```
sqlite                      → running 1 test  … 0 assertion(s) active
sqlite,tenancy,manage       → running 2 tests … 1 assertion(s) active
sqlite,tenancy,manage,mcp   → running 3 tests … 2 assertion(s) active
```

The count and the test name are visible without `--nocapture`, which is what a reader actually sees.

**So:** fail only when a feature set is genuinely *required*. When it is merely *sufficient* — the usual case in a crate with real feature combinations, and the distinction is usually unexamined — report instead.

This is the same shape as the previous rule: **make the degraded case say so, rather than make the degraded case impossible.** Impossible is usually too strong for the real feature matrix.

### Prefer a gate that can't drift over one that checks for drift

A test comparing two representations is weaker than a design with only one representation. `docs_contract.rs` exists because prose and code are genuinely separate artifacts — but where a single source can serve both, that beats any checker.

## Severity taxonomy

Rank by what it costs a reader, not by how wrong it feels:

| Class | Meaning |
|---|---|
| **Code, not docs** | The doc is right and the code doesn't do it. File an issue; don't touch the prose |
| **Won't compile** | The example fails on paste. Highest reader cost — it stops someone at the moment they decided to try |
| **Backwards** | Stated as the inverse of reality. Costliest to read past: a confident wrong statement doesn't prompt anyone to check |
| **Stale** | True once. Renames, shipped roadmap items still in future tense, drifted version pins |
| **Missing** | Shipped surface nothing documents. A CLI reference omitting a third of its verbs is how people conclude a feature doesn't exist |

**Not findings:** typos, grammar, style, "this section could be longer". They dilute the list and get the real items ignored.

A doc that diagnoses a problem correctly and then prescribes an ineffective remedy is **worse than silence** — the reader does the recommended thing and stops looking. Rank those above a plain omission.

## Making a page testable

The backlog lives in `crates/rustango/tests/docs_contract.rs`: **17 published pages, 157 Rust examples that nothing compiles or runs.** The list may only shrink, and a page on it may not gain new examples.

To pay one down:

1. Write a test exercising the page's examples against the real API.
2. Declare it in the module header — this is what links test to page:
   ```rust
   //! Backing test for `docs/auth-jwt.md` — standalone HS256 JWTs. Pure, no DB.
   ```
3. Delete that page's row from `UNBACKED`.
4. `cargo test -p rustango --test docs_contract` — it fails if you removed the wrong row.

Where examples are illustrative rather than runnable, use a **compile gate** instead: see `examples/getting_started_blog/tests/guide_crud_compiles.rs`. It's wired into the `sqlite_litmus` CI job, so it runs without Postgres — which means it also catches a Postgres-only method presented as portable, the single most common backend error in these docs.

## Translations

Every `docs/*.md` has `docs/de/`, `docs/fr/`, `docs/es/` mirrors. **A fix that doesn't touch all four creates drift.**

Detecting existing drift needs the right method:

- Heading counts, line counts and `wc -l` show **zero drift** across all 38 pages × 3 locales. Misleading — they're in near-lockstep.
- A raw code-block diff gives **~200 false positives**, because translators correctly localize the comments inside code blocks.
- **What works:** extract fenced code blocks and inline code spans, **strip comments**, then diff. That surfaced an entire auth section and a config key missing from all three locales while every structural metric said clean.

Never localize a literal that describes syntax — `formfield_overrides = "field:widget"` stays English; a French `"champ:widget"` renders as copy-pasteable and wrong.

## Existing guards

Complementary; don't duplicate them:

| Test | Enforces |
|---|---|
| `docs_links.rs` | Every link, image and anchor resolves, in every locale |
| `docs_versions.rs` | Sample output shows the shipping version (note: its adjacency rule misses two-component pins like `version = "0.44"`) |
| `docs_contract.rs` | Backing-test coverage, and untested pages gain no new examples |

## Reference

The full audit — 107 findings with doc line, source line, and which side is wrong — is in `.claude/docs-audit/`, indexed by `README.md` and tiered A–E. Findings carry stable IDs (`A-01`, `C-14`); cite them in commits and issues.

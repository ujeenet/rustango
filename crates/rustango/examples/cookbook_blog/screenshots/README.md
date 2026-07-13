# Cookbook screenshots — capture runbook & conventions

The cookbook's visual chapters (Admin, Forms, Templates, tenancy UI) embed real
screenshots; code/CLI chapters embed real captured command/output blocks. **No
fabricated images, ever** — every screenshot is produced by the steps below.

## Where they live / naming
- All images: `crates/rustango/examples/cookbook_blog/screenshots/`.
- Name: `chNN-<slug>.png` (e.g. `ch08-admin-changelist.png`), matching the
  chapter number in `COOKBOOK.md`.
- Embed from `COOKBOOK.md` with a relative path + alt text:
  `![Admin changelist for posts](screenshots/ch08-admin-changelist.png)`.

## Capturing a UI screenshot (admin / forms / templates)
The app surfaces are rendered by a real server + a real browser:

1. **Postgres** (throwaway):
   `docker run -d --name cb-shot-pg -e POSTGRES_USER=rustango -e POSTGRES_PASSWORD=rustango -e POSTGRES_DB=cookbook -p 55433:5432 postgres:16-alpine`
2. **Boot a non-tenanted admin/template server** bound to a plain port. The
   admin is a plain axum router (`admin::Builder::new(pool).build()`), so a
   small harness binary can serve it at `127.0.0.1:PORT` with seeded blog data —
   no host-based tenancy to fight. (`admin_demo` demonstrates the exact shape:
   `RUSTANGO_BIND=127.0.0.1:PORT DATABASE_URL=… cargo run` → `/admin`,
   `/admin/<table>` changelists.)
3. **Capture** with a headless browser (Playwright): navigate to the page,
   full-page PNG, save into this directory under the `chNN-<slug>.png` name.

## Capturing evidence for code/CLI chapters (no UI)
Non-visual chapters use a **real captured output block** instead of an image —
paste the actual command + its output (or the passing test line), e.g.:

```console
$ cargo run -- migrate
applying migration=0001_initial
  applied 0001_initial
```

## Regenerating
Screenshots go stale when the admin theme or a page changes. Re-run the steps
above for the affected chapter; keep the filenames stable so `COOKBOOK.md`
references don't break.

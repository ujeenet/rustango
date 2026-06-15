# admin_demo — runnable companion to `docs/admin.md`

A focused showcase of the auto-admin. `Author`, `Tag`, `Post`, and `Comment`
models map onto Postgres; `Post` carries a rich `admin(...)` block, the server
registers two bulk **actions** (`publish` / `archive`), `Comment` is shown as a
read-only **inline** on the post page, and `Post` has an **audit trail**. The
app self-seeds (15 posts + comments) on first boot so the admin has data.

In-repo example → path dep on the framework. A real project uses
`rustango = "0.43"` from crates.io.

## What maps to which admin feature

| Where | Feature |
|---|---|
| `src/lib.rs` — `Post`'s `admin(...)` block | list_display, list_display_links, list_filter, search_fields, search_help_text, ordering, list_per_page, date_hierarchy, fieldsets, actions |
| `src/lib.rs` — `register_admin_inline!` | the comments inline on the post page |
| `src/lib.rs` — `audit(track = …)` | the audit-trail card on the post page |
| `src/main.rs` — `Builder::register_action(...)` | the `publish` / `archive` bulk-action handlers |
| `tests/admin_smoke.rs` | asserts the list/detail render with filters + inline |

## Run it

```bash
docker compose -f ../getting_started_blog/docker-compose.yml up -d postgres  # or any Postgres
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/admin_demo
cargo run -- migrate     # create tables (+ the audit-log table)
cargo run                # seeds on first boot; admin at http://localhost:8080/admin
cargo test               # integration test (needs DATABASE_URL)
```

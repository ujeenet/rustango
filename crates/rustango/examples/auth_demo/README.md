# auth_demo

Companion example for the rustango **Authentication** docs (`docs/auth-*.md`).

Every authentication flow has a dedicated deep-dive doc, and each doc's code
snippets are copied from a compiled, CI-tested file here:

| Doc | Backing test |
|-----|--------------|
| `docs/auth-passwords.md` | `tests/auth_passwords.rs` |
| `docs/auth-sessions.md`  | `tests/auth_sessions.rs`  |

More flows (JWT, API keys, OAuth2, TOTP, passkey, …) land in later batches.

## Run

```sh
# one flow
cargo test --test auth_passwords

# everything (what CI runs)
cargo run -- migrate        # apply migrations (needs DATABASE_URL → Postgres)
cargo test

# under SQLite (no server; pure/in-memory tests don't need a DB)
cargo test --no-default-features --features sqlite --test auth_passwords --test auth_sessions
```

`DATABASE_URL` defaults to the project `.env`; copy `.env.example` if present.

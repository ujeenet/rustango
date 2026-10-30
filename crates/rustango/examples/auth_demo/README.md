# auth_demo

Companion example for the rustango **Authentication** docs (`docs/auth-*.md`).

Every authentication flow has a dedicated deep-dive doc, and each doc's code
snippets are copied from a compiled, CI-tested file here:

| Doc | Backing test |
|-----|--------------|
| `docs/auth-passwords.md`  | `tests/auth_passwords.rs`  |
| `docs/auth-sessions.md`   | `tests/auth_sessions.rs`   |
| `docs/auth-jwt.md`        | `tests/auth_jwt.rs`        |
| `docs/auth-jwt-api.md`    | `tests/auth_jwt_api.rs`    |
| `docs/auth-decorators.md` | `tests/auth_decorators.rs` |

More flows (API keys, OAuth2, TOTP, passkey, …) land in later batches.

## Run

```sh
# one flow
cargo test --test auth_passwords

# everything (what CI runs)
cargo test

# under SQLite (no server; pure/in-memory tests don't need a DB)
cargo test --no-default-features --features sqlite --test auth_passwords --test auth_sessions
```

The flow tests above need no database. To create the `auth_users` table:

```sh
cp .env.example .env     # set DATABASE_URL; the database must exist
cargo run -- migrate     # applies migrations/ and the framework's system/ chain

# or on SQLite, no server needed
DATABASE_URL='sqlite:./auth_demo.db?mode=rwc' \
  cargo run --no-default-features --features sqlite -- migrate
```

After changing `src/models.rs`, run `cargo run -- makemigrations` and commit
the files it writes.

# cargo-rustango

Project scaffolder for the [`rustango`](https://crates.io/crates/rustango) web framework.

```sh
cargo install cargo-rustango
cargo rustango new myblog
```

Run `cargo rustango new` with no arguments to get a wizard that asks for each
option and then prints the equivalent command line.

## Templates

| `--template` | What you get |
|---|---|
| `api` | Bare ORM + axum, no admin — JSON-only services. |
| `fullstack` *(default)* | ORM + auto-admin. |
| `tenant` | Multi-tenancy, operator console, and the `tenancy_manage` CLI. |

## Options

| Flag | Meaning |
|---|---|
| `-t, --template <name>` | Template from the table above. Default `fullstack`. |
| `-b, --backend <name>` | `postgres` (default), `sqlite`, or `mysql`. |
| `-F, --features <list>` | Extra rustango features, comma-separated. |
| `-i, --interactive` | Pick from numbered menus instead of passing flags. |
| `--rustango-path <dir>` | Depend on a local rustango checkout rather than the published crate. |

`--backend` decides what `cargo run` uses and shapes `.env.example`,
`docker-compose.yml`, and the settings tiers to match. The other two stay one
flag away:

```sh
cargo run --no-default-features --features sqlite
```

`--features` accepts any feature the framework exposes — `tenancy`, `csrf`,
`sso`, `passkey`, `cache-redis`, `jobs`, `scheduler`, `email-smtp`, `mcp` and
others. `cargo rustango new --help` lists the full set with one-line
descriptions.

## Examples

```sh
cargo rustango new myblog                      # fullstack on postgres
cargo rustango new api_demo --template api     # JSON-only service
cargo rustango new shop --template tenant      # multi-tenant
cargo rustango new edge --backend sqlite
```

## Note

This crate is standalone: it writes project source files and does not link to
any rustango runtime types, so it never has to match your project's framework
version.

## License

MIT OR Apache-2.0. See the [parent repository](https://github.com/ujeenet/rustango)
for the canonical README and full project documentation.

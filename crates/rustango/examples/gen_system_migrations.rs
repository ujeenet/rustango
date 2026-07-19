//! Dev tool — (re)author the framework's shipped **system-app** migrations
//! into `crates/rustango/system/migrations/`, which the crate then embeds via
//! `embed_migrations!` and applies to every downstream project.
//!
//! Run FROM the framework crate dir with a *superset* feature set so the shipped
//! schema covers every consumer (e.g. `admin-sso` columns), then commit the
//! result:
//!
//! ```bash
//! cd crates/rustango
//! cargo run --example gen_system_migrations --features tenancy,admin-sso
//! ```
//!
//! Each framework schema change → re-run this, commit the new migration.
//! Projects NEVER generate these; they apply the embedded chain.
use rustango::core::ModelScope;
use std::path::Path;

fn main() {
    // `make_migrations_system` writes to `<root>/system/migrations/`; run from
    // the crate dir so that's `crates/rustango/system/migrations/`.
    let root = Path::new(".");
    for scope in [ModelScope::Registry, ModelScope::Tenant] {
        match rustango::migrate::make_migrations_system(root, scope, None) {
            Ok(Some(m)) => println!("wrote system/migrations/{}.json ({scope:?} scope)", m.name),
            Ok(None) => println!("no changes for {scope:?} scope"),
            Err(e) => {
                eprintln!("gen_system_migrations failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

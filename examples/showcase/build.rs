// Rebuild when a migration file is added, so embed_migrations! sees it.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}

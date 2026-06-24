//! Regression guard for the scaffolder-bootstrap-drift dogfood finding.
//!
//! `cargo-rustango` embeds a *static copy* of the registry + tenant
//! bootstrap migrations (`crates/cargo-rustango/templates/0001_*.json`) so a
//! fresh `cargo rustango new --template tenant` project is `migrate`-ready out
//! of the box. That copy silently rotted behind the framework — it was missing
//! the FK `RelationSnapshot` metadata on the auth tables AND the five MCP agent
//! tables — so `makemigrations` on a fresh project failed with
//! `field metadata changed ... None -> Some(fk)`.
//!
//! This test pins the embedded copy to what the current framework's
//! `init_tenancy` actually produces, so the scaffolder can't drift again. If
//! it fails, regenerate: run `cargo run -- init-tenancy` in a scratch tenant
//! project and copy the JSON into `crates/cargo-rustango/templates/`.
#![cfg(feature = "tenancy")]

use std::path::PathBuf;

use rustango::tenancy::bootstrap;

fn embedded(name: &str) -> serde_json::Value {
    // Sibling crate, relative to this crate's manifest dir (in-repo only).
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../cargo-rustango/templates")
        .join(name);
    let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    serde_json::from_str(&raw).expect("embedded template is valid JSON")
}

#[test]
fn scaffolder_embedded_bootstrap_matches_framework() {
    let dir = std::env::temp_dir().join(format!("rustango_bootstrap_check_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    bootstrap::init_tenancy(&dir).expect("init_tenancy generates fresh bootstrap");

    for name in [
        "0001_rustango_registry_initial.json",
        "0001_rustango_tenant_initial.json",
    ] {
        let raw = std::fs::read_to_string(dir.join(name)).expect("read generated bootstrap");
        let fresh: serde_json::Value = serde_json::from_str(&raw).expect("generated is valid JSON");
        assert_eq!(
            fresh,
            embedded(name),
            "crates/cargo-rustango/templates/{name} is STALE vs the framework's `init_tenancy` \
             output. Regenerate it: `cargo run -- init-tenancy` in a scratch tenant project, then \
             copy the JSON into crates/cargo-rustango/templates/."
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

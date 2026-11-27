//! The shipped system files match today's models. `RUSTANGO_WRITE_SYSTEM=1`
//! rewrites them (and `files.rs`); a build with every feature checks them.

use super::*;

#[cfg(all(
    feature = "tenancy",
    feature = "admin",
    feature = "sso",
    feature = "admin-sso",
    feature = "media",
    feature = "passkey",
    feature = "mcp",
    feature = "totp"
))]
#[test]
fn shipped_system_files_match_the_models() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/migrate/v2/system");
    let generated = super::gen::generate().unwrap();
    if std::env::var_os("RUSTANGO_WRITE_SYSTEM").is_some() {
        let mut index = String::from("&[\n");
        for (app, stem, text) in &generated {
            std::fs::create_dir_all(dir.join(app)).unwrap();
            std::fs::write(dir.join(app).join(format!("{stem}.json")), text).unwrap();
            index.push_str(&format!(
                "    (\"{app}\", \"{stem}\", include_str!(\"{app}/{stem}.json\")),\n"
            ));
        }
        index.push_str("]\n");
        std::fs::write(dir.join("files.rs"), index).unwrap();
        return;
    }
    let shipped: Vec<(String, String, String)> = FILES
        .iter()
        .map(|(a, s, t)| ((*a).to_owned(), (*s).to_owned(), (*t).to_owned()))
        .collect();
    assert_eq!(
        shipped.len(),
        generated.len(),
        "run with RUSTANGO_WRITE_SYSTEM=1 to regenerate"
    );
    for (s, g) in shipped.iter().zip(&generated) {
        assert_eq!(s, g, "run with RUSTANGO_WRITE_SYSTEM=1 to regenerate");
    }
}

/// Every shipped file loads and the whole system project replays.
#[test]
fn shipped_system_files_load() {
    let files = files().unwrap();
    let _ = files.len();
}

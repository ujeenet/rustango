//! Golden checksum fixtures. The hashes come from an independent
//! implementation of the canonical form and are frozen: changing one breaks
//! every ledger that recorded it.

/// `(file name, text, checksum alg 1)`.
pub(super) const CHECKSUM_GOLDENS: &[(&str, &str, &str)] = &[
    (
        "0002_create_invoice.json",
        include_str!("fixtures/0002_create_invoice.json"),
        "8b7d1d469f8a9e20e98e53f4f59c5bf0ca81c109ade1aa258cbc98f3c91cfa60",
    ),
    (
        "0003_pg_trgm_index.json",
        include_str!("fixtures/0003_pg_trgm_index.json"),
        "15931edc378442f4df21ccdad484ecfb6219adb093d4e13ae4efa9cab432155f",
    ),
    (
        "0004_escapes.json",
        include_str!("fixtures/0004_escapes.json"),
        "957b934b7ede49688a36ef0b9c51aada9c4f5a0f36276bd1c3bade3f5941848f",
    ),
    (
        "0012_add_locale_to_user.json",
        include_str!("fixtures/0012_add_locale_to_user.json"),
        "7287f388ba3d9d02910fcab9c72e865b86d6d2f08df7fc0322c0f5a3ee2899b6",
    ),
];

/// Every fixture is a valid v2 file, and the loader records the golden checksum.
#[test]
fn fixtures_load_with_their_golden_checksum() {
    for (name, text, want) in CHECKSUM_GOLDENS {
        let loaded = super::load::load_file(std::path::Path::new(name), text).unwrap();
        assert_eq!(loaded.checksum, *want, "{name}");
    }
}

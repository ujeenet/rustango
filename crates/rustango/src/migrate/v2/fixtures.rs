//! Golden checksum fixtures. The hashes come from an independent
//! implementation of the canonical form and are frozen: changing one breaks
//! every ledger that recorded it.

/// `(file name, text, checksum alg 1)`.
pub(super) const CHECKSUM_GOLDENS: &[(&str, &str, &str)] = &[
    (
        "0002_create_invoice.json",
        include_str!("fixtures/0002_create_invoice.json"),
        "19a833ec731b8c9d60cf16bfeb9a548605692cb685e741834dada4daed8eeb02",
    ),
    (
        "0003_pg_trgm_index.json",
        include_str!("fixtures/0003_pg_trgm_index.json"),
        "15931edc378442f4df21ccdad484ecfb6219adb093d4e13ae4efa9cab432155f",
    ),
    (
        "0004_escapes.json",
        include_str!("fixtures/0004_escapes.json"),
        "3cc4be3a093f651369c6ec1676a12123a8b0f8eedaf8b90e32c9fc4d57d80f36",
    ),
    (
        "0012_add_locale_to_user.json",
        include_str!("fixtures/0012_add_locale_to_user.json"),
        "7287f388ba3d9d02910fcab9c72e865b86d6d2f08df7fc0322c0f5a3ee2899b6",
    ),
    (
        "0013_portable_defaults.json",
        include_str!("fixtures/0013_portable_defaults.json"),
        "9a2f8f84467c710fccbe18c5d712a3df62278a0483f79420e4e953910355068c",
    ),
    (
        "0014_adopt_column.json",
        include_str!("fixtures/0014_adopt_column.json"),
        "a7779609726e6218dc183f145c022c92683b66e9d787a5a4a2406098aab30702",
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

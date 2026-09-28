//! Admin TOTP codes are single use on every backend (#1672).

#![cfg(all(
    feature = "admin",
    feature = "totp",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use rustango::admin::totp_store::{self, AdminTotp};
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::totp::{self, TotpSecret};
use rustango::tri_dialect_test;

const UID: i64 = 41;

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<AdminTotp>(pool).await;
    // On a table that already has the column, ensure must still succeed.
    totp_store::ensure_table(pool).await.expect("ensure_table");
}

async fn enroll(pool: &Pool) -> TotpSecret {
    let secret = TotpSecret::generate();
    totp_store::start_enrollment(pool, UID, &secret)
        .await
        .unwrap();
    totp_store::confirm(pool, UID).await.unwrap();
    secret
}

async fn a_code_is_accepted_once(pool: &Pool) {
    let secret = enroll(pool).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let code = totp::generate_at(&secret, now, 30, 6);
    assert!(totp_store::redeem_code(pool, UID, &secret, &code)
        .await
        .unwrap());
    assert!(
        !totp_store::redeem_code(pool, UID, &secret, &code)
            .await
            .unwrap(),
        "a TOTP code was accepted twice"
    );
    // A still-valid code from an earlier step is refused too.
    let earlier = totp::generate_at(&secret, now - 30, 30, 6);
    if earlier != code {
        assert!(!totp_store::redeem_code(pool, UID, &secret, &earlier)
            .await
            .unwrap());
    }
    assert!(!totp_store::redeem_code(pool, UID, &secret, "abcdef")
        .await
        .unwrap());
}

async fn concurrent_redemptions_accept_one(pool: &Pool) {
    let secret = enroll(pool).await;
    let code = totp::generate(&secret, 30, 6);
    let (a, b) = tokio::join!(
        totp_store::redeem_code(pool, UID, &secret, &code),
        totp_store::redeem_code(pool, UID, &secret, &code),
    );
    assert!(
        a.unwrap() ^ b.unwrap(),
        "exactly one concurrent redemption must win"
    );
}

/// A table from before `last_used_step` still reads, and gains the
/// column on the first redemption, keeping the device.
async fn an_old_table_gains_the_column(pool: &Pool) {
    let secret = enroll(pool).await;
    drop_last_used_step(pool).await;
    let got = totp_store::confirmed_secret_checked(pool, UID)
        .await
        .expect("an old table still reads")
        .expect("the device survives");
    assert_eq!(got.to_base32(), secret.to_base32());
    let code = totp::generate(&secret, 30, 6);
    assert!(totp_store::redeem_code(pool, UID, &secret, &code)
        .await
        .unwrap());
    assert!(!totp_store::redeem_code(pool, UID, &secret, &code)
        .await
        .unwrap());
}

/// Back to the pre-#1672 table shape.
async fn drop_last_used_step(pool: &Pool) {
    drop_column(pool, "last_used_step").await;
}

async fn drop_column(pool: &Pool, column: &str) {
    let snapshot = rustango::migrate::SchemaSnapshot::from_models_forced(&[AdminTotp::SCHEMA]);
    let drop = rustango::migrate::render_changes_split_with_dialect(
        &[rustango::migrate::SchemaChange::DropColumn {
            table: AdminTotp::SCHEMA.table.to_owned(),
            column: column.to_owned(),
        }],
        &snapshot,
        pool.dialect(),
    )
    .unwrap();
    for stmt in &drop.immediate {
        rustango::sql::raw_execute_pool(pool, stmt, Vec::new())
            .await
            .unwrap();
    }
}

/// `ensure_table` alone upgrades an old table.
async fn ensure_table_adds_the_column(pool: &Pool) {
    enroll(pool).await;
    drop_last_used_step(pool).await;
    totp_store::ensure_table(pool).await.expect("ensure_table");
    let steps: Vec<Option<i64>> = AdminTotp::objects()
        .filter("user_id", UID)
        .values_list_flat("last_used_step")
        .fetch(pool)
        .await
        .expect("ensure_table must add last_used_step");
    assert_eq!(steps, vec![None]);
}

/// A code for another secret does not redeem against this device.
async fn a_code_redeems_only_against_the_stored_secret(pool: &Pool) {
    enroll(pool).await;
    let other = TotpSecret::generate();
    let code = totp::generate(&other, 30, 6);
    assert!(!totp_store::redeem_code(pool, UID, &other, &code)
        .await
        .unwrap());
}

/// Enrollment confirms and burns the code in one write.
async fn confirming_burns_the_code(pool: &Pool) {
    let secret = TotpSecret::generate();
    totp_store::start_enrollment(pool, UID, &secret)
        .await
        .unwrap();
    let code = totp::generate(&secret, 30, 6);
    assert!(totp_store::confirm_with_code(pool, UID, &secret, &code)
        .await
        .unwrap());
    assert!(totp_store::confirmed_secret_checked(pool, UID)
        .await
        .unwrap()
        .is_some());
    assert!(!totp_store::redeem_code(pool, UID, &secret, &code)
        .await
        .unwrap());
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// #1756 — an unfinished re-enroll keeps the old factor gating login.
async fn a_pending_reenroll_keeps_the_old_factor(pool: &Pool) {
    let old = enroll(pool).await;
    let new = TotpSecret::generate();
    totp_store::start_enrollment(pool, UID, &new).await.unwrap();
    let gate = totp_store::confirmed_secret_checked(pool, UID)
        .await
        .unwrap()
        .expect("a pending re-enroll dropped the confirmed factor");
    assert_eq!(gate.to_base32(), old.to_base32());
    let t = now();
    assert!(
        !totp_store::redeem_code(pool, UID, &new, &totp::generate_at(&new, t, 30, 6))
            .await
            .unwrap()
    );
    assert!(
        totp_store::redeem_code(pool, UID, &old, &totp::generate_at(&old, t, 30, 6))
            .await
            .unwrap()
    );
    // A second restart keeps it too, and the page offers the new secret.
    let newer = TotpSecret::generate();
    totp_store::start_enrollment(pool, UID, &newer)
        .await
        .unwrap();
    let dev = totp_store::device(pool, UID).await.unwrap();
    assert!(dev.confirmed);
    assert_eq!(dev.secret_base32, old.to_base32());
    assert_eq!(dev.pending_secret().unwrap().to_base32(), newer.to_base32());
}

/// #1756 — confirming swaps the secret; the old one stops working.
async fn finishing_a_reenroll_swaps_the_secret(pool: &Pool) {
    let old = enroll(pool).await;
    let t = now();
    // The old code signs in at this step first: the new one still confirms.
    assert!(
        totp_store::redeem_code(pool, UID, &old, &totp::generate_at(&old, t, 30, 6))
            .await
            .unwrap()
    );
    let new = TotpSecret::generate();
    totp_store::start_enrollment(pool, UID, &new).await.unwrap();
    let wrong = totp::generate_at(&old, t + 30, 30, 6);
    assert!(!totp_store::confirm_with_code(pool, UID, &new, &wrong)
        .await
        .unwrap());
    let code = totp::generate_at(&new, t, 30, 6);
    assert!(totp_store::confirm_with_code(pool, UID, &new, &code)
        .await
        .unwrap());
    let gate = totp_store::confirmed_secret_checked(pool, UID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate.to_base32(), new.to_base32());
    assert!(totp_store::device(pool, UID)
        .await
        .unwrap()
        .pending_secret()
        .is_none());
    assert!(
        !totp_store::redeem_code(pool, UID, &old, &totp::generate_at(&old, t + 30, 30, 6))
            .await
            .unwrap(),
        "the old secret still signs in after the re-enroll"
    );
    assert!(!totp_store::redeem_code(pool, UID, &new, &code)
        .await
        .unwrap());
    assert!(
        totp_store::redeem_code(pool, UID, &new, &totp::generate_at(&new, t + 30, 30, 6))
            .await
            .unwrap()
    );
}

/// A table from before `pending_secret_base32` gains it on `ensure_table`.
async fn ensure_table_adds_the_pending_column(pool: &Pool) {
    let old = enroll(pool).await;
    drop_column(pool, "pending_secret_base32").await;
    assert!(totp_store::confirmed_secret_checked(pool, UID)
        .await
        .unwrap()
        .is_some());
    totp_store::ensure_table(pool).await.expect("ensure_table");
    let new = TotpSecret::generate();
    totp_store::start_enrollment(pool, UID, &new).await.unwrap();
    let gate = totp_store::confirmed_secret_checked(pool, UID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate.to_base32(), old.to_base32());
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        a_code_is_accepted_once,
        concurrent_redemptions_accept_one,
        an_old_table_gains_the_column,
        ensure_table_adds_the_column,
        a_code_redeems_only_against_the_stored_secret,
        confirming_burns_the_code,
        a_pending_reenroll_keeps_the_old_factor,
        finishing_a_reenroll_swaps_the_secret,
        ensure_table_adds_the_pending_column,
    ],
}

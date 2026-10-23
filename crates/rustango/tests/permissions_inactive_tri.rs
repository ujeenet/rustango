//! A deactivated user holds no permission through a role, on every backend.

#![cfg(feature = "tenancy")]

use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::permissions::{
    assign_role_pool, create_role_pool, grant_role_perm_pool, has_all_perms_pool,
    has_any_perm_pool, set_user_perm_pool, user_permissions_pool,
};
use rustango::tenancy::User;
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

/// An inactive user with a role grant and a direct grant.
async fn inactive_user_with_grants(pool: &Pool) -> i64 {
    let mut u = User {
        username: unique("inact"),
        active: false,
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("user");
    let uid = *u.id.get().expect("pk");
    let role = create_role_pool(&unique("r"), "", pool).await.expect("role");
    grant_role_perm_pool(role, "pit_post.view", pool)
        .await
        .expect("role grant");
    assign_role_pool(uid, role, pool).await.expect("assign");
    set_user_perm_pool(uid, "pit_post.export", true, pool)
        .await
        .expect("direct grant");
    uid
}

async fn inactive_user_holds_no_grants(pool: &Pool) {
    let uid = inactive_user_with_grants(pool).await;
    let perms = user_permissions_pool(uid, pool).await.expect("perms");
    assert!(perms.is_empty(), "{perms:?}");
    let any = has_any_perm_pool(uid, &["pit_post.view"], pool)
        .await
        .expect("any");
    let all = has_all_perms_pool(uid, &["pit_post.view"], pool)
        .await
        .expect("all");
    assert!(!any && !all);
    // The PostgreSQL-only forms agree.
    #[cfg(feature = "postgres")]
    if let Some(pg) = pool.as_postgres() {
        use rustango::tenancy::permissions::{has_all_perms, has_any_perm, user_permissions};
        assert!(!has_any_perm(uid, &["pit_post.view"], pg).await.unwrap());
        assert!(!has_all_perms(uid, &["pit_post.view"], pg).await.unwrap());
        assert!(user_permissions(uid, pg).await.unwrap().is_empty());
    }
}

/// The control: the same grants on an active user count.
async fn active_user_holds_its_grants(pool: &Pool) {
    let uid = inactive_user_with_grants(pool).await;
    let mut u: User = User::objects()
        .where_(User::id.eq(uid))
        .fetch(pool)
        .await
        .expect("fetch")
        .pop()
        .expect("user");
    u.active = true;
    u.save_pool(pool).await.expect("activate");
    let mut perms = user_permissions_pool(uid, pool).await.expect("perms");
    perms.sort();
    assert_eq!(perms, ["pit_post.export", "pit_post.view"]);
    #[cfg(feature = "postgres")]
    if let Some(pg) = pool.as_postgres() {
        use rustango::tenancy::permissions::{has_all_perms, has_any_perm};
        assert!(has_any_perm(uid, &["pit_post.view"], pg).await.unwrap());
        assert!(has_all_perms(uid, &["pit_post.view", "pit_post.export"], pg)
            .await
            .unwrap());
    }
}

tri_dialect_test!(
    setup: setup,
    scenarios: [inactive_user_holds_no_grants, active_user_holds_its_grants],
);

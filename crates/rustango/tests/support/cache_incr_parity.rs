//! The `incr` and text-value rules every cache backend shares.

use rustango::cache::{Cache, CacheError};

/// `cache` follows the `Cache::incr` and `Cache::set` rules; `name` labels failures.
pub async fn assert_incr_rules(cache: &dyn Cache, name: &str) {
    // (stored, by, result): only the exact `i64` text is a counter.
    let cases: [(&str, i64, i64); 11] = [
        ("7", 1, 8),
        ("5\n", 1, 1),
        ("-5", 1, -4),
        ("0", 2, 2),
        ("+7", 1, 1),
        ("007", 1, 1),
        (" 5", 1, 1),
        ("5 ", 1, 1),
        ("-0", 1, 1),
        ("abc", 5, 5),
        ("99999999999999999999", 1, 1),
    ];
    for (stored, by, want) in cases {
        let key = format!("parity:{stored}");
        cache.set(&key, stored, None).await.expect("set");
        let got = cache.incr(&key, by, None).await;
        assert_eq!(got.ok(), Some(want), "{name}: incr({stored:?}, {by})");
        assert_eq!(
            cache.get(&key).await.expect("get").as_deref(),
            Some(want.to_string().as_str()),
            "{name}: stored after incr({stored:?})"
        );
    }

    // Overflow is an error both ways and leaves the counter as it was.
    for (start, by) in [(i64::MAX, 1), (i64::MIN, -1)] {
        let key = format!("parity:edge{by}");
        cache
            .set(&key, &start.to_string(), None)
            .await
            .expect("set");
        let err = cache.incr(&key, by, None).await.expect_err(name);
        assert!(err.to_string().contains("overflow"), "{name}: {err}");
        assert_eq!(
            cache.get(&key).await.expect("get").as_deref(),
            Some(start.to_string().as_str()),
            "{name}: overflow changed the counter"
        );
    }

    // A NUL is refused by every text writer, and nothing is stored.
    let nul = "a\0b";
    assert!(
        matches!(
            cache.set("parity:nul", nul, None).await,
            Err(CacheError::Serialization(_))
        ),
        "{name}: set with NUL"
    );
    assert!(
        matches!(
            cache.add("parity:nul", nul, None).await,
            Err(CacheError::Serialization(_))
        ),
        "{name}: add with NUL"
    );
    assert!(
        matches!(
            cache.set_forever("parity:nul", nul).await,
            Err(CacheError::Serialization(_))
        ),
        "{name}: set_forever with NUL"
    );
    assert_eq!(cache.get("parity:nul").await.expect("get"), None, "{name}");
}

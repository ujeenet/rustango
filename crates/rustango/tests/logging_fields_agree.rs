//! The two request layers must name the same things the same way
//! (#1480).
//!
//! `access_log` and `tracing_layer` both describe an HTTP request, and
//! before this guard they agreed on exactly two field names out of six:
//!
//! | concept | access_log | tracing_layer |
//! |---|---|---|
//! | method | `method` | `http.request.method` |
//! | path | `path` | `url.path` |
//! | status | `status` | `http.response.status_code` |
//! | client IP | `ip` | *(absent)* |
//! | duration | `duration_ms` | `duration_ms` |
//! | tenant | `tenant` (`-` when none) | `tenant` (omitted when none) |
//!
//! An app running both logged every request twice under two schemas.
//! Nothing caught it because the unit tests on both sides assert
//! configuration — redaction lists, thresholds, IP resolution — and
//! never the emitted field names. Eighteen `access_log` tests pass
//! unchanged across the rename that this file exists to pin, which is
//! the tell.
//!
//! ## Why a source scan rather than captured output
//!
//! `tracing` field names must be literals at the macro site, so they
//! cannot be lifted into shared constants and made identical by
//! construction — which would have been the better fix. Capturing real
//! output needs a custom subscriber plus a live server for the tenant
//! path. The names are literals in the source, so reading the source
//! asks the question directly and cannot pass vacuously: both files
//! must be found, and both must be non-trivial.

use std::path::{Path, PathBuf};

fn src(name: &str) -> String {
    let p: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The vocabulary both layers must use, in OpenTelemetry's HTTP
/// semantic conventions.
const SHARED: &[&str] = &[
    "http.request.method",
    "url.path",
    "http.response.status_code",
    "duration_ms",
    "tenant",
];

#[test]
fn both_request_layers_use_the_same_field_names() {
    let access = src("access_log.rs");
    let tracing = src("tracing_layer.rs");

    // Non-vacuity: if either file stopped describing requests, every
    // `contains` below would still need to hold for a reason.
    assert!(
        access.contains("tracing::info!") && tracing.contains("span!"),
        "this guard is reading the wrong files — access_log should emit events \
         and tracing_layer should open a span"
    );

    let mut missing = Vec::new();
    for field in SHARED {
        if !access.contains(field) {
            missing.push(format!("access_log.rs does not use `{field}`"));
        }
        if !tracing.contains(field) {
            missing.push(format!("tracing_layer.rs does not use `{field}`"));
        }
    }
    assert!(
        missing.is_empty(),
        "the two request layers have drifted apart again:\n  {}\n\nBoth describe the \
         same HTTP request. When they disagree, an app running both logs every \
         request twice under two schemas, and a dashboard built on one silently \
         misses the other.",
        missing.join("\n  ")
    );
}

/// The short names are gone from the access log, not merely joined by
/// the long ones.
///
/// Checked separately because `contains` on the long name would pass
/// just as happily if both spellings were emitted side by side — which
/// would be the worst outcome: two names for one value, in one line.
#[test]
fn the_access_log_does_not_still_emit_the_short_names() {
    let access = src("access_log.rs");

    // The emitted forms, not the words. `method` appears all over the
    // file as a local variable and in prose; `method = %method` is the
    // field.
    const LEGACY: &[&str] = &["method = %method", "path = %path", "ip = ip.as_deref()"];

    let found: Vec<&str> = LEGACY
        .iter()
        .copied()
        .filter(|l| access.contains(l))
        .collect();
    assert!(
        found.is_empty(),
        "access_log still emits the pre-#1480 field names:\n  {}\n\nThe OpenTelemetry \
         names replaced these; emitting both would put two spellings of one value on \
         the same line.",
        found.join("\n  ")
    );
}

//! The two request layers must name the same things the same way
//! (#1480).
//!
//! `access_log` and `tracing_layer` both describe an HTTP request, and
//! before the rename they agreed on two field names out of six —
//! `method`/`path`/`status` against
//! `http.request.method`/`url.path`/`http.response.status_code`. An app
//! running both logged every request twice under two schemas, and no
//! test noticed, because every test on both sides asserts configuration
//! (redaction lists, thresholds, IP resolution) and none asserts an
//! emitted field name.
//!
//! ## The first version of this file could not detect that
//!
//! It did `file.contains("http.request.method")` over the whole source.
//! Every name it looked for also appears in the module doc-comment, so
//! the prose alone satisfied it: renaming all six *emitted* fields to
//! `verb` / `route` / `code` / … left both tests green. The negative
//! test hardcoded three exact legacy spellings, so any new wrong name
//! passed it too — and `http_method = %method` contains
//! `method = %method`, so it false-positived on a name that was fine.
//!
//! The header claimed source-scanning "cannot pass vacuously". It could,
//! and it did. This version reads **only the macro invocations** and
//! compares the field-name sets, so prose cannot vouch for code and an
//! unknown wrong name fails by absence rather than by blocklist.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn src(name: &str) -> String {
    let p: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The text of every `macro!( … )` invocation whose name is in `names`.
///
/// Brace-counting rather than a line scan: these span many lines, and
/// the point is to see the argument list and nothing else — doc
/// comments, local bindings and prose are all outside it.
fn macro_bodies(src: &str, names: &[&str]) -> String {
    let mut out = String::new();
    for name in names {
        let needle = format!("{name}(");
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(&needle) {
            let open = from + rel + needle.len() - 1;
            // Skip a match inside a doc comment: if the line starts with
            // `//`, it is prose about the macro, not the macro.
            let line_start = src[..open].rfind('\n').map_or(0, |i| i + 1);
            if src[line_start..open].trim_start().starts_with("//") {
                from = open + 1;
                continue;
            }
            let mut depth = 0i32;
            let mut end = open;
            for (i, c) in src[open..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push_str(&src[open..=end]);
            out.push('\n');
            from = end + 1;
        }
    }
    out
}

/// Field names bound in a macro body: `"a.b" = …` and bare `ident,`.
fn field_names(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in body.lines() {
        let t = line.trim().trim_end_matches(',');
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        // `"http.request.method" = %method`
        if let Some(rest) = t.strip_prefix('"') {
            if let Some((name, after)) = rest.split_once('"') {
                if after.trim_start().starts_with('=') {
                    out.insert(name.to_owned());
                }
                continue;
            }
        }
        // `duration_ms` / `tenant = %tenant` — a bare or assigned ident.
        let ident: String = t
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if ident.is_empty() {
            continue;
        }
        let after = t[ident.len()..].trim_start();
        if after.is_empty() || after.starts_with('=') {
            out.insert(ident);
        }
    }
    out
}

fn access_log_fields() -> BTreeSet<String> {
    let s = src("access_log.rs");
    field_names(&macro_bodies(&s, &["tracing::info!", "tracing::warn!"]))
}

fn tracing_layer_fields() -> BTreeSet<String> {
    let s = src("tracing_layer.rs");
    let mut f = field_names(&macro_bodies(&s, &["info_span!"]));
    // `span.record("x", v)` is the same field, filled in later.
    for (i, m) in s.match_indices(".record(\"") {
        let rest = &s[i + m.len()..];
        if let Some((name, _)) = rest.split_once('"') {
            f.insert(name.to_owned());
        }
    }
    f
}

/// The vocabulary both layers must use, in OpenTelemetry's HTTP
/// semantic conventions.
const SHARED: &[&str] = &[
    "http.request.method",
    "url.path",
    "url.query",
    "http.response.status_code",
    "duration_ms",
    "tenant",
];

#[test]
fn both_request_layers_emit_the_same_field_names() {
    let access = access_log_fields();
    let tracing = tracing_layer_fields();

    // Non-vacuity, and this time it means something: the sets come from
    // macro arguments only, so an empty one means the scan broke rather
    // than that the code is fine.
    assert!(
        access.len() >= 5,
        "parsed {} fields out of access_log's emit macros — the scan is broken, so \
         a pass here would prove nothing: {access:?}",
        access.len()
    );
    assert!(
        tracing.len() >= 5,
        "parsed {} fields out of tracing_layer's span — the scan is broken: {tracing:?}",
        tracing.len()
    );

    let missing: Vec<String> = SHARED
        .iter()
        .flat_map(|f| {
            let mut v = Vec::new();
            if !access.contains(*f) {
                v.push(format!("access_log does not emit `{f}`"));
            }
            if !tracing.contains(*f) {
                v.push(format!("tracing_layer does not emit `{f}`"));
            }
            v
        })
        .collect();

    assert!(
        missing.is_empty(),
        "the two request layers have drifted apart again:\n  {}\n\naccess_log emits \
         {access:?}\ntracing_layer emits {tracing:?}\n\nBoth describe the same HTTP \
         request. When they disagree, an app running both logs every request twice \
         under two schemas, and a dashboard built on one silently misses the other.",
        missing.join("\n  ")
    );
}

/// Neither layer may emit a request field outside the shared vocabulary.
///
/// This replaces a blocklist of three known-bad spellings, which by
/// construction could only catch the names someone had already thought
/// of. An allowlist fails on *any* unknown name, which is what the drift
/// actually looks like.
#[test]
fn neither_layer_emits_a_field_outside_the_agreed_set() {
    // Fields that legitimately belong to one layer only.
    const PER_LAYER: &[&str] = &[
        "client.address",           // access_log: the peer
        "http.response.body.size",  // tracing_layer: from the response
        "org_id",                   // tracing_layer: numeric tenant id
        "trace_id",                 // tracing_layer: W3C traceparent
        "span_id",                  // tracing_layer: W3C traceparent
        "parent_span_id",           // tracing_layer: W3C traceparent
        "otel.kind",                // tracing_layer: OTel span kind
        "otel.name",                // tracing_layer: OTel span name
        "trace_flags",              // tracing_layer: W3C traceparent
        "user_agent.original",      // tracing_layer: request header
        "network.protocol.version", // tracing_layer: HTTP version
    ];
    let allowed: BTreeSet<&str> = SHARED.iter().chain(PER_LAYER.iter()).copied().collect();

    let mut stray = Vec::new();
    for (layer, fields) in [
        ("access_log", access_log_fields()),
        ("tracing_layer", tracing_layer_fields()),
    ] {
        for f in &fields {
            if !allowed.contains(f.as_str()) {
                stray.push(format!("{layer} emits `{f}`, which is in neither set"));
            }
        }
    }

    assert!(
        stray.is_empty(),
        "unrecognised request-log fields:\n  {}\n\nEither it is part of the shared \
         vocabulary — add it to SHARED and emit it from both layers — or it belongs \
         to one layer, in which case add it to PER_LAYER with a comment saying why.",
        stray.join("\n  ")
    );
}

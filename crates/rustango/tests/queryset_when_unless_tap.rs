//! Eloquent-shape conditional / side-effect builder helpers on
//! `QuerySet`: `.when(cond, f)` / `.unless(cond, f)` / `.tap(f)`.
//! Unit tests verify the conditional semantics without touching
//! a real DB — the goal is to assert the closure ran or didn't
//! based on the boolean flag.

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "wut_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub status: String,
}

#[test]
fn when_true_runs_closure_and_when_false_skips() {
    // Sentinel: the closure pushes a filter; whether it fired is
    // observable by the SQL the queryset compiles to. We don't
    // compile here (would need a dialect) — instead use a captured
    // `Cell<bool>` that flips when the closure runs.
    use std::cell::Cell;
    let fired = Cell::new(false);
    let _qs = Post::objects().when(true, |qs| {
        fired.set(true);
        qs.filter("status", "draft")
    });
    assert!(fired.get(), "when(true) must run the closure");

    let fired = Cell::new(false);
    let _qs = Post::objects().when(false, |qs| {
        fired.set(true);
        qs.filter("status", "draft")
    });
    assert!(!fired.get(), "when(false) must NOT run the closure");
}

#[test]
fn unless_inverts_when_semantics() {
    use std::cell::Cell;
    let fired = Cell::new(false);
    let _qs = Post::objects().unless(false, |qs| {
        fired.set(true);
        qs.filter("status", "draft")
    });
    assert!(fired.get(), "unless(false) must run the closure");

    let fired = Cell::new(false);
    let _qs = Post::objects().unless(true, |qs| {
        fired.set(true);
        qs.filter("status", "draft")
    });
    assert!(!fired.get(), "unless(true) must NOT run the closure");
}

#[test]
fn tap_runs_side_effect_and_returns_self_unchanged() {
    use std::cell::Cell;
    let observed = Cell::new(false);
    let _qs = Post::objects()
        .filter("status", "draft")
        .tap(|_qs| observed.set(true));
    assert!(observed.get());
}

#[test]
fn when_chains_compose_naturally() {
    // Build a queryset whose final shape depends on runtime
    // flags. The point of `.when()` is that this stays in
    // fluent-builder shape instead of breaking into imperative
    // mut-shadowing.
    let only_drafts = true;
    let with_author: Option<i64> = Some(42);
    let _qs = Post::objects()
        .when(only_drafts, |q| q.filter("status", "draft"))
        .when(with_author.is_some(), |q| {
            q.filter("id", with_author.unwrap_or_default())
        });
}

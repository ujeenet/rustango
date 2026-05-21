//! Django-parity #415 — `setting_changed` signal fires from
//! [`rustango::test_settings::with_overridden`] on scope enter +
//! exit.

#![cfg(all(feature = "signals", feature = "config"))]

use std::sync::Arc;

use rustango::config::Settings;
use rustango::signals::setting::{
    clear_all, connect_setting_changed, receiver_count, SettingChangedContext,
};
use rustango::test_settings::with_overridden;
use tokio::sync::Mutex;

/// Suite-wide lock — `clear_all` touches a global registry.
fn suite_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn signal_fires_on_overlay_enter_and_exit() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<SettingChangedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_setting_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let value = with_overridden(Settings::default(), async {
        // While inside the scope, we should have observed exactly one
        // "enter" fire and zero "exit" fires.
        let so_far = captured.lock().await.clone();
        assert_eq!(so_far.len(), 1, "expected 1 enter fire mid-scope");
        assert!(so_far[0].enter);
        42
    })
    .await;
    assert_eq!(value, 42);

    // After the scope returns, we should have seen the exit fire too.
    let got = captured.lock().await;
    assert_eq!(got.len(), 2, "expected enter+exit fires, got: {got:?}");
    assert!(got[0].enter);
    assert!(!got[1].enter);
}

#[tokio::test]
async fn nested_scopes_fire_signal_pairs() {
    let _g = suite_lock().lock().await;
    clear_all();

    let trace: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = trace.clone();
    connect_setting_changed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx.enter);
        }
    });

    with_overridden(Settings::default(), async {
        with_overridden(Settings::default(), async {
            // Inner scope active here.
        })
        .await;
    })
    .await;

    let got = trace.lock().await;
    // Sequence: outer enter, inner enter, inner exit, outer exit.
    assert_eq!(*got, vec![true, true, false, false]);
}

#[tokio::test]
async fn no_receivers_does_not_break_overlay() {
    let _g = suite_lock().lock().await;
    clear_all();
    assert_eq!(receiver_count(), 0);

    // Override scope with no receivers connected must run cleanly.
    let value = with_overridden(Settings::default(), async { 7 }).await;
    assert_eq!(value, 7);
}

#[tokio::test]
async fn disconnect_stops_future_fires() {
    let _g = suite_lock().lock().await;
    clear_all();

    let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let sink = counter.clone();
    let id = connect_setting_changed(move |_| {
        let sink = sink.clone();
        async move {
            *sink.lock().await += 1;
        }
    });

    with_overridden(Settings::default(), async {}).await;
    assert_eq!(*counter.lock().await, 2, "enter+exit while connected");

    assert!(rustango::signals::setting::disconnect_setting_changed(id));

    with_overridden(Settings::default(), async {}).await;
    assert_eq!(
        *counter.lock().await,
        2,
        "no further fires after disconnect"
    );
}

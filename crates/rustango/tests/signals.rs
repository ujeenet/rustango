//! Unit tests for the signals dispatcher — no DB required.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rustango::signals::{
    clear_all, connect_post_delete, connect_post_save, connect_pre_delete, connect_pre_save,
    disconnect_post_save, disconnect_pre_save, receiver_count, send_post_delete, send_post_save,
    send_pre_delete, send_pre_save, PostSaveContext,
};

// ------------------------------------------------------------------ Fixtures
//
// Each test uses its own `*Sig` model type so the global registry doesn't
// leak state between tests. (clear_all() is also called per test to be safe.)

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_pre_save_models")]
pub struct PreSaveSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    pub name: String,
}

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_post_save_models")]
pub struct PostSaveSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    pub name: String,
}

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_pre_delete_models")]
pub struct PreDeleteSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
}

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_post_delete_models")]
pub struct PostDeleteSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
}

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_disconnect_models")]
pub struct DisconnectSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
}

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_isolated_models")]
pub struct IsolatedSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
}

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_other_models")]
pub struct OtherSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
}

#[derive(rustango::Model, Clone)]
#[rustango(table = "sig_count_models")]
pub struct CountSig {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
}

fn instance<T: rustango::core::Model>() -> rustango::sql::Auto<i64> {
    rustango::sql::Auto::Set(1)
}

// Serialize all tests against the global registry by holding a mutex across
// each test. clear_all() between tests is not enough since the registry is
// truly global.
fn registry_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

// ------------------------------------------------------------------ pre_save

#[tokio::test]
async fn pre_save_receiver_fires() {
    let _g = registry_lock().lock().unwrap();
    clear_all();

    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    connect_pre_save::<PreSaveSig, _, _>(move |_inst| {
        let c = c.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });

    let m = PreSaveSig { id: instance::<PreSaveSig>(), name: "x".into() };
    send_pre_save(&m).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------------------------ post_save

#[tokio::test]
async fn post_save_receiver_gets_context() {
    let _g = registry_lock().lock().unwrap();
    clear_all();

    let captured = Arc::new(Mutex::new(None::<bool>));
    let cap = captured.clone();
    connect_post_save::<PostSaveSig, _, _>(move |_inst, ctx| {
        let cap = cap.clone();
        async move {
            *cap.lock().unwrap() = Some(ctx.created);
        }
    });

    let m = PostSaveSig { id: instance::<PostSaveSig>(), name: "y".into() };
    send_post_save(&m, PostSaveContext { created: true }).await;
    assert_eq!(*captured.lock().unwrap(), Some(true));

    send_post_save(&m, PostSaveContext { created: false }).await;
    assert_eq!(*captured.lock().unwrap(), Some(false));
}

// ------------------------------------------------------------------ pre_delete

#[tokio::test]
async fn pre_delete_receiver_fires() {
    let _g = registry_lock().lock().unwrap();
    clear_all();

    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();
    connect_pre_delete::<PreDeleteSig, _, _>(move |_inst| {
        let f = f.clone();
        async move { f.fetch_add(1, Ordering::SeqCst); }
    });

    let m = PreDeleteSig { id: instance::<PreDeleteSig>() };
    send_pre_delete(&m).await;
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------------------------ post_delete

#[tokio::test]
async fn post_delete_receiver_fires() {
    let _g = registry_lock().lock().unwrap();
    clear_all();

    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();
    connect_post_delete::<PostDeleteSig, _, _>(move |_inst| {
        let f = f.clone();
        async move { f.fetch_add(1, Ordering::SeqCst); }
    });

    let m = PostDeleteSig { id: instance::<PostDeleteSig>() };
    send_post_delete(&m).await;
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------------------------ multiple receivers

#[tokio::test]
async fn multiple_receivers_run_in_registration_order() {
    let _g = registry_lock().lock().unwrap();
    clear_all();

    let order = Arc::new(Mutex::new(Vec::<usize>::new()));

    for i in 0..3 {
        let o = order.clone();
        connect_pre_save::<PreSaveSig, _, _>(move |_inst| {
            let o = o.clone();
            async move { o.lock().unwrap().push(i); }
        });
    }

    let m = PreSaveSig { id: instance::<PreSaveSig>(), name: "n".into() };
    send_pre_save(&m).await;
    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
}

// ------------------------------------------------------------------ disconnect

#[tokio::test]
async fn disconnect_removes_receiver() {
    let _g = registry_lock().lock().unwrap();
    clear_all();

    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();
    let id = connect_pre_save::<DisconnectSig, _, _>(move |_inst| {
        let f = f.clone();
        async move { f.fetch_add(1, Ordering::SeqCst); }
    });

    let m = DisconnectSig { id: instance::<DisconnectSig>() };
    send_pre_save(&m).await;
    assert_eq!(fired.load(Ordering::SeqCst), 1);

    let removed = disconnect_pre_save::<DisconnectSig>(id);
    assert!(removed);

    send_pre_save(&m).await;
    assert_eq!(fired.load(Ordering::SeqCst), 1, "removed receiver must not fire");
}

#[tokio::test]
async fn disconnect_unknown_id_returns_false() {
    let _g = registry_lock().lock().unwrap();
    clear_all();
    // No receivers registered — disconnecting an unrelated id should be a clean false.
    let id = connect_post_save::<DisconnectSig, _, _>(|_, _| async {});
    let other_id = connect_post_save::<DisconnectSig, _, _>(|_, _| async {});
    assert!(disconnect_post_save::<DisconnectSig>(id));
    assert!(disconnect_post_save::<DisconnectSig>(other_id));
    // Now the bag is empty — re-disconnecting returns false.
    assert!(!disconnect_post_save::<DisconnectSig>(id));
}

// ------------------------------------------------------------------ isolation between types

#[tokio::test]
async fn signals_are_isolated_per_model_type() {
    let _g = registry_lock().lock().unwrap();
    clear_all();

    let isolated_fired = Arc::new(AtomicUsize::new(0));
    let other_fired = Arc::new(AtomicUsize::new(0));

    let i_f = isolated_fired.clone();
    connect_pre_save::<IsolatedSig, _, _>(move |_inst| {
        let f = i_f.clone();
        async move { f.fetch_add(1, Ordering::SeqCst); }
    });
    let o_f = other_fired.clone();
    connect_pre_save::<OtherSig, _, _>(move |_inst| {
        let f = o_f.clone();
        async move { f.fetch_add(1, Ordering::SeqCst); }
    });

    // Fire only IsolatedSig
    let m = IsolatedSig { id: instance::<IsolatedSig>() };
    send_pre_save(&m).await;
    assert_eq!(isolated_fired.load(Ordering::SeqCst), 1);
    assert_eq!(other_fired.load(Ordering::SeqCst), 0, "OtherSig receiver must NOT fire");
}

// ------------------------------------------------------------------ receiver_count

#[tokio::test]
async fn receiver_count_tracks_registrations() {
    let _g = registry_lock().lock().unwrap();
    clear_all();
    assert_eq!(receiver_count::<CountSig>(), 0);

    connect_pre_save::<CountSig, _, _>(|_| async {});
    connect_post_save::<CountSig, _, _>(|_, _| async {});
    connect_post_delete::<CountSig, _, _>(|_| async {});
    assert_eq!(receiver_count::<CountSig>(), 3);

    clear_all();
    assert_eq!(receiver_count::<CountSig>(), 0);
}

//! Unit tests for issue #827 — `Observer<T>` trait grouping all four
//! lifecycle hooks under one struct, and the `without_signals` /
//! `save_quietly` / `delete_quietly` task-local suppression scope.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rustango::signals::{
    clear_all, connect_post_save, connect_pre_save, disconnect_observer, observe, receiver_count,
    save_quietly, send_post_delete, send_post_save, send_pre_delete, send_pre_save,
    without_signals, Observer, PostSaveContext, ReceiverFuture,
};
use rustango::Auto;
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "so_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

// Process-global mutex: signals share a single registry; running
// these tests in parallel with each other (or with anything else
// that mutates the registry) is racy.
fn signal_lock() -> &'static Mutex<()> {
    static M: tokio::sync::OnceCell<Mutex<()>> = tokio::sync::OnceCell::const_new();
    // OnceCell::const_new + sync init via blocking_lock isn't
    // ergonomic — fall back to OnceLock with std::sync::Mutex would
    // serialize sync; use a static + once init.
    static G: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    let _ = M; // suppress dead-code lint on the unused OnceCell
    G.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn observer_receives_all_four_events() {
    let _g = signal_lock().lock().await;
    clear_all();

    #[derive(Default)]
    struct Counter {
        pre_save: AtomicUsize,
        post_save: AtomicUsize,
        pre_delete: AtomicUsize,
        post_delete: AtomicUsize,
    }
    impl Observer<Post> for Arc<Counter> {
        fn pre_save(&self, _i: Arc<Post>) -> ReceiverFuture {
            self.pre_save.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn post_save(&self, _i: Arc<Post>, _ctx: PostSaveContext) -> ReceiverFuture {
            self.post_save.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn pre_delete(&self, _i: Arc<Post>) -> ReceiverFuture {
            self.pre_delete.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn post_delete(&self, _i: Arc<Post>) -> ReceiverFuture {
            self.post_delete.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }
    let counter: Arc<Counter> = Arc::new(Counter::default());
    let handle = observe::<Post, _>(Arc::clone(&counter));
    assert_eq!(receiver_count::<Post>(), 4, "observe wires all 4 signals");

    let post = Post {
        id: Auto::Set(1),
        title: "x".into(),
    };
    send_pre_save(&post).await;
    send_post_save(&post, PostSaveContext { created: true }).await;
    send_pre_delete(&post).await;
    send_post_delete(&post).await;

    assert_eq!(counter.pre_save.load(Ordering::SeqCst), 1);
    assert_eq!(counter.post_save.load(Ordering::SeqCst), 1);
    assert_eq!(counter.pre_delete.load(Ordering::SeqCst), 1);
    assert_eq!(counter.post_delete.load(Ordering::SeqCst), 1);

    let removed = disconnect_observer::<Post>(&handle);
    assert_eq!(removed, 4, "disconnect_observer detaches all 4");
    assert_eq!(receiver_count::<Post>(), 0);
}

#[tokio::test]
async fn without_signals_suppresses_dispatch() {
    let _g = signal_lock().lock().await;
    clear_all();

    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&counter);
    let _id = connect_pre_save::<Post, _, _>(move |_i| {
        let c = Arc::clone(&c2);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });

    let post = Post {
        id: Auto::Set(1),
        title: "x".into(),
    };

    // Baseline: fires once.
    send_pre_save(&post).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // Inside `without_signals`: no dispatch.
    without_signals(async {
        send_pre_save(&post).await;
        send_pre_save(&post).await;
    })
    .await;
    assert_eq!(counter.load(Ordering::SeqCst), 1, "suppressed inside scope");

    // After the scope: dispatch resumes.
    send_pre_save(&post).await;
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    clear_all();
}

#[tokio::test]
async fn save_quietly_is_alias_for_without_signals() {
    let _g = signal_lock().lock().await;
    clear_all();

    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&counter);
    let _id = connect_post_save::<Post, _, _>(move |_i, _ctx| {
        let c = Arc::clone(&c2);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });

    let post = Post {
        id: Auto::Set(1),
        title: "x".into(),
    };
    save_quietly(async {
        send_post_save(&post, PostSaveContext { created: true }).await;
    })
    .await;
    assert_eq!(counter.load(Ordering::SeqCst), 0, "save_quietly suppresses");

    clear_all();
}

#[tokio::test]
async fn nested_without_signals_scopes_compose() {
    let _g = signal_lock().lock().await;
    clear_all();

    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&counter);
    let _id = connect_pre_save::<Post, _, _>(move |_i| {
        let c = Arc::clone(&c2);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });

    let post = Post {
        id: Auto::Set(1),
        title: "x".into(),
    };
    without_signals(async {
        send_pre_save(&post).await; // suppressed
        without_signals(async {
            send_pre_save(&post).await; // still suppressed inside nested
        })
        .await;
        send_pre_save(&post).await; // outer scope still suppresses
    })
    .await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // Outside any scope: fires.
    send_pre_save(&post).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    clear_all();
}

#[tokio::test]
async fn observer_with_default_noop_methods_drops_irrelevant_events() {
    let _g = signal_lock().lock().await;
    clear_all();

    // Only override post_save; the other three remain default no-op.
    struct OnlyPostSave {
        seen: Arc<AtomicUsize>,
    }
    impl Observer<Post> for OnlyPostSave {
        fn post_save(&self, _i: Arc<Post>, _ctx: PostSaveContext) -> ReceiverFuture {
            self.seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let _handle = observe::<Post, _>(OnlyPostSave {
        seen: Arc::clone(&seen),
    });

    let post = Post {
        id: Auto::Set(1),
        title: "x".into(),
    };
    send_pre_save(&post).await;
    send_post_save(&post, PostSaveContext { created: false }).await;
    send_pre_delete(&post).await;
    send_post_delete(&post).await;

    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "only post_save should have observed"
    );

    clear_all();
}

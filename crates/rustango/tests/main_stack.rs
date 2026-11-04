//! `#[rustango::main]` runs its body on a thread named `main` with an
//! 8 MiB stack, even when called from a thread with 1 MiB (Windows' main).

#![cfg(feature = "runtime")]

use std::sync::atomic::{AtomicBool, Ordering};

static RAN_ON_MAIN: AtomicBool = AtomicBool::new(false);

/// Uses 1.5 to 3 MiB of stack: overflows 1 MiB, fits in 8 MiB.
#[inline(never)]
fn deep(n: u32) -> u64 {
    let pad = std::hint::black_box([n as u8; 16 * 1024]);
    if n == 0 {
        u64::from(pad[0])
    } else {
        deep(n - 1) + u64::from(pad[pad.len() - 1])
    }
}

#[rustango::main(logging = false)]
async fn entry() -> Result<(), String> {
    RAN_ON_MAIN.store(
        std::thread::current().name() == Some("main"),
        Ordering::SeqCst,
    );
    std::hint::black_box(deep(96));
    Err("boom".to_owned())
}

#[rustango::main(logging = false, flavor = "current_thread")]
async fn panics() {
    panic!("main body panicked");
}

/// Run `f` on a thread with 1 MiB of stack, as Windows runs `main`.
fn on_small_stack<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::Result<T> {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
}

#[test]
fn the_body_runs_on_a_big_stack_and_keeps_the_exit_code() {
    let code = on_small_stack(entry).expect("no overflow, no panic");
    assert!(
        RAN_ON_MAIN.load(Ordering::SeqCst),
        "body not on the `main` thread"
    );
    // The `Err` still reports a failure exit code.
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::FAILURE)
    );
}

#[test]
fn a_panic_in_the_body_resumes_with_its_message() {
    let payload = on_small_stack(panics).expect_err("the panic must reach the caller");
    let msg = payload.downcast_ref::<&str>().copied().unwrap_or_default();
    assert_eq!(msg, "main body panicked");
}

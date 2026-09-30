//! `catch_unwind` for a future, without a `futures` dependency.

use std::future::Future;

/// Drive `fut` to completion, turning a panic in any single `poll`
/// into an `Err`.
pub(crate) async fn catch_unwind<F: Future>(fut: F) -> std::thread::Result<F::Output> {
    use std::task::Poll;
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    })
    .await
}

/// The message of a caught panic, when it carried a string.
#[cfg(feature = "jobs")]
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

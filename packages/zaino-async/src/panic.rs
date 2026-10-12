//! Turning a panic into a value at a supervision boundary.
//!
//! Not `unsafe` in the memory-safety sense — this module contains no `unsafe`
//! code (the crate is `#![forbid(unsafe_code)]`). What it deals with is
//! *unwind-safety*: [`std::panic::catch_unwind`] asks the caller to acknowledge,
//! via [`AssertUnwindSafe`](std::panic::AssertUnwindSafe), that observing a value
//! after a panic will not expose logically-broken (but still memory-safe) state.
//! The seam here is meant for supervision, where a caught panic leads to
//! *teardown*, never to resuming over the panicked future's state.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::future::FutureExt;

/// Extract a panic's message from its payload — the `Box<dyn Any>` a join
/// (`JoinError::into_panic`) or a [`catch_panic`] hands back. Mirrors the panic
/// hook's downcast (`zaino_logging`), so a panic's origin log and wherever it is
/// later reconciled render the same text. Shared so every supervision boundary
/// that turns a panic into a value says the same thing.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

/// Run `fut` to completion, capturing a panic as a value instead of unwinding:
/// `Ok(output)` if it finished, `Err(message)` if it panicked (the panic hook
/// still logs the origin; this returns the same [`panic_message`] text).
///
/// The seam a supervision boundary uses to turn a panicking run loop into a
/// *reported* failure rather than a silent task death — so a component maps the
/// outcome to its status instead of re-deriving `catch_unwind` at each site.
///
/// This is safe code (no `unsafe`). It does apply
/// [`AssertUnwindSafe`](std::panic::AssertUnwindSafe) internally, which is an
/// assertion about *unwind-safety* (logical, not memory): sound **only** where a
/// caught panic leads to teardown, never to resuming over `fut`'s
/// partially-updated state — exactly the supervised-component contract (a panic
/// escalates and the component is torn down).
pub async fn catch_panic<F: Future>(fut: F) -> Result<F::Output, String> {
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(output) => Ok(output),
        Err(payload) => Err(panic_message(&*payload)),
    }
}

#[cfg(test)]
mod tests {
    use super::{catch_panic, panic_message};

    #[tokio::test]
    async fn catches_a_panic_as_its_message() {
        let caught = catch_panic(async { panic!("boom at the seam") }).await;
        assert_eq!(caught, Err("boom at the seam".to_owned()));
    }

    #[tokio::test]
    async fn passes_through_a_normal_result() {
        let ok: Result<Result<u8, ()>, String> = catch_panic(async { Ok::<_, ()>(42u8) }).await;
        assert_eq!(ok, Ok(Ok(42)));
    }

    #[test]
    fn renders_a_str_payload() {
        let message = std::panic::catch_unwind(|| panic!("literal")).unwrap_err();
        assert_eq!(panic_message(&*message), "literal");
    }
}

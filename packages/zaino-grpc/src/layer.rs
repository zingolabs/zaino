//! `take_ready`: tower `poll_ready` → `call` handoff shared by the serve-path layers

/// `inner` swapped for its clone: the instance polled ready is the one called (tower's contract)
pub(crate) fn take_ready<Inner: Clone>(inner: &mut Inner) -> Inner {
    let clone = inner.clone();
    std::mem::replace(inner, clone)
}

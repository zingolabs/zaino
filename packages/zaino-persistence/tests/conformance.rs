//! The port's conformance suite against the in-memory backend.

use proptest::prelude::*;
use zaino_persistence::{
    conformance::{contract, history, steps, Subject, SPECS},
    in_memory::InMemoryBackend,
};

/// One shared store: a reopen is a fresh handle onto the same data.
struct InMemory(InMemoryBackend);

impl InMemory {
    fn new() -> Self {
        Self(InMemoryBackend::new(&SPECS).expect("conformance specs are distinct"))
    }
}

impl Subject for InMemory {
    type Backend = InMemoryBackend;

    fn open(&self) -> InMemoryBackend {
        self.0.clone()
    }

    fn keeps_unflushed(&self) -> bool {
        true
    }
}

#[test]
fn in_memory_contract() {
    contract(InMemory::new());
}

proptest! {
    #[test]
    fn in_memory_history(steps in steps()) {
        history(InMemory::new(), &steps);
    }
}

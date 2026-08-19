# `zaino-status` — usage

How a Zaino component reports whether it is working. A leaf crate whose only
dependency is `tracing`.

## Why it is its own crate

Status is the one thing *every* subsystem has, including the ones whose whole
purpose is to depend on as little as possible. While this vocabulary lived in
`zaino-common`, saying "I am syncing" cost a dependency on the validator config,
the logging stack, TLS and `zebra-chain` — the entire graph of a general-purpose
crate, to publish an enum.

So the dependency list here is `tracing` and nothing else, and **the crate stays
that way**. This is vocabulary, not machinery: if a change wants a new
dependency, the thing it is trying to express probably belongs in the crate that
already has that dependency.

## Use

```rust
use zaino_status::{NamedAtomicStatus, Status, StatusType};

// Report a status by implementing the trait.
impl Status for MyService {
    fn status(&self) -> StatusType {
        self.status.load()
    }
}

// Or hold one directly. Cloning shares the cell.
let status = NamedAtomicStatus::new("MyService", StatusType::Spawning);
status.store(StatusType::Ready);
```

`Liveness` and `Readiness` arrive for free: they are blanket impls over
`Status`, so a `T: Status` bound is everything a caller needs to ask both the
questions an operator or orchestrator actually asks.

## `probing` cannot be split out

`status` and `probing` look separable — one is the vocabulary, the other the two
health questions — but `impl<T: Status> Liveness for T` needs one of the two
traits to be local to the crate defining it. Splitting them leaves that blanket
impl homeless, and every implementor would have to write `Liveness` and
`Readiness` by hand. They ship together for that reason, not by accident.

## `NamedAtomicStatus` names the component for the log, not for the API

Every transition is logged with the component name, so one subsystem's lifecycle
can be followed through an interleaved log without correlating by anything else.
The name is `&'static str` and is not part of any identity: two cells with the
same name are still two cells, and clones of one cell are the same cell — every
clone observes and publishes the same status. That sharing is what lets a
service hand read handles to consumers while its own task keeps writing.

There is no unnamed variant. `AtomicStatus` existed and was deleted: an
untraceable status transition is not worth the type.

## Related

- `zaino-consensus` — the other leaf extracted from `zaino-common` for the same
  reason, and with the same standing constraint on its dependency list.

# zaino-async

Low-level async/tokio building blocks shared across Zaino, one layer **below**
the component and supervision model (`zaino-component`). Domain-free: no Zcash, no
indexing — only concurrency plumbing. Depending on `tokio` here is intended; the
rule it upholds is that the tokio dependency stays confined to infrastructure
crates and never reaches the domain layer (`zaino-primitives`, `zaino-core`).

## `Task<T = ()>`

A named `tokio` task that renders panics coherently.

```rust,no_run
# async fn demo() {
use zaino_async::{Task, TaskName};

// Fire-and-forget: `T` defaults to `()`.
let worker = Task::spawn(TaskName("babysitter"), |cancel| async move {
    cancel.cancelled().await; // cooperative stop
});
worker.cancel();

// Value-returning: joined for its result.
let sum = Task::spawn(TaskName("adder"), |_cancel| async move { 6 * 7 });
assert_eq!(sum.join().await.unwrap(), 42);
# }
```

`join()` yields the task's output, or a [`TaskError`] that names the failing task
by **our** [`TaskName`] — never tokio's opaque runtime task id — and preserves a
panic's message, so a caller placing it on a health `reason` still says *what*
failed. This is the single home for the `JoinError` → typed-error rendering that
was otherwise re-derived (and sometimes stringified) at each spawn site.

`spawn` hands the body a [`CancellationToken`] to poll for a cooperative stop;
`cancel()` requests that, `abort()` stops the task hard, `is_finished()` checks
without awaiting.

## Intended scope (filled in as we go)

This crate is the home for the async patterns Zaino currently open-codes inline.
As each earns a second caller it moves here, defined and tested once:

- bounded, order-preserving concurrency runner (N fetches in flight, first-error
  aborts) — today hand-rolled in the source provisioner;
- `spawn_blocking` with the same panic-rendered join, for CPU/DB offload;
- interval-poll-into-a-`watch` loop;
- typed `timeout`/deadline wrappers;
- watch fan-in (`select` over many status receivers);
- the generic backoff scheduler underlying source retry (classification stays
  with the source).

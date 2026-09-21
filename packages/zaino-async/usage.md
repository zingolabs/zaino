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

`join()` yields the task's output, or a [`TaskError`]. This crate is the single
home for rendering a `tokio` `JoinError` as a typed, named error — see
[`TaskError`] for what it preserves and why.

`spawn` hands the body a [`CancellationToken`] to poll for a cooperative stop;
`cancel()` requests that, `abort()` stops the task hard, `is_finished()` checks
without awaiting.

## `run_blocking`

Run synchronous, blocking work off the async worker threads — the [`Task::join`]
panic treatment for a closure that cannot be made async (e.g. a synchronous disk
read).

```rust,no_run
# async fn demo() {
use zaino_async::{run_blocking, TaskName};

let bytes = run_blocking(TaskName("read-file"), || std::fs::read("/etc/hostname"))
    .await
    .expect("no panic");
# let _ = bytes;
# }
```

A blocking closure cannot be cooperatively cancelled, so `run_blocking` takes no
[`CancellationToken`]; it renders a panic as a named [`TaskError`] and otherwise
returns the closure's value.

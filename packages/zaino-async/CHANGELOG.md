# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. The low-level async layer, one level below the component model:
  domain-free `tokio` plumbing, with no Zcash or indexing vocabulary.
- `Task<T = ()>` — a named `tokio` task with cooperative cancellation, a hard
  `abort`, and panic capture on `join`. Generic over its output, defaulting to
  `()` for the fire-and-forget case.
- `TaskError` — renders a `tokio` `JoinError` by the task's own `TaskName`
  rather than the opaque runtime task id, and preserves a panic's message, so a
  join failure names what died.
- `run_blocking` — run a blocking closure on the runtime's blocking pool,
  rendering a panic as a named `TaskError` just as `Task::join` does. For
  synchronous work that cannot be made async (e.g. disk I/O) and so must not run
  on an async worker thread.
- `catch_panic` / `panic_message` — run a future and turn a panic into a value.
- Re-exports `CancellationToken`, so a consumer naming it in a signature need not
  depend on `tokio-util` directly.

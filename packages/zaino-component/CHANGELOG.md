# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. The component abstraction: a supervised in-process subsystem, and
  what every subsystem otherwise hand-rolls — `ComponentStatus`, `Lifecycle`,
  `Health`, `Managed`, and `Task`.
- A component's state has two independent axes. `Lifecycle` is a management
  phase, moved only by `Managed`; `Health` is a condition the component reaches
  on its own. Neither overwrites the other, so `ComponentStatus` reports both
  rather than collapsing them into one verdict.
- Reading and driving are separate capabilities. `StatusSource` is universal and
  synchronous, so a supervisor samples every component without awaiting.
  `Managed` is implemented only by components the runtime owns, which is what
  distinguishes them from observed dependencies such as a validator.
- `Lifecycle::try_advance_to` owns the state machine. A restart is a full lap
  through `Closing → Offline → Spawning`, never a jump back to running, and an
  illegal transition is the typed `IllegalTransition` rather than a silent
  correction.
- `Task` wraps `tokio::spawn` with a typed name, cooperative cancellation, a
  hard abort, and panic capture surfaced as `TaskError`.

# `zaino-component` — usage

A **component** is a supervised in-process subsystem. This crate standardises
what every subsystem otherwise hand-rolls: what it reports about itself, how it
is driven, and the tasks it runs.

## Two axes, read independently

A component's state has two parts, and they do not constrain each other.

| axis | is | moves when |
|---|---|---|
| [`Lifecycle`] | a management **phase** | management drives it — spawn advances it, stop retires it |
| [`Health`] | a reported **condition** | the component's own circumstances change: a dependency drops, a task panics |

A component that is `Ready` may be `Critical`; one that is `Offline` may have
been `Healthy` when it stopped. Neither axis overwrites the other, and
[`ComponentStatus`] reports both without reconciling them. Everything else in
this crate follows from that split: it is why status is a snapshot rather than a
verdict, and why the read side and the control side are separate traits.

## Two capabilities: observed and owned

Reading a component's state and driving it are separate, so a consumer bounds on
exactly what it uses.

- [`StatusSource`] is universal. Every component reports a [`ComponentStatus`],
  and the runtime observes all of them.
- [`Managed`] is the line between an **owned** component and an **observed**
  one. The runtime drives the lifecycle of what it owns. An external dependency
  — a validator, say — reports its status but cannot be spawned or stopped by
  the runtime, so it implements [`StatusSource`] and not [`Managed`].

[`StatusSource::status`] is synchronous and cheap by contract: a supervisor
samples every component without awaiting. A component that publishes
[`StatusWatch`] lets a supervisor react to transitions instead of polling.

```rust
use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle, StatusSource};

struct Validator;

// Observed, not owned: it reports, the runtime cannot drive it.
impl StatusSource for Validator {
    fn status(&self) -> ComponentStatus {
        ComponentStatus::new(ComponentName("validator"), Lifecycle::Ready, Health::Healthy)
    }
}
```

## Two altitudes: tasks and components

`zaino_async::Task` is the low primitive — one named async task with cooperative
cancellation, a hard abort, and panic capture on join. It lives one layer below,
in [`zaino-async`], because it is domain-free concurrency plumbing that
non-component code (e.g. the source provisioner's fetch pump) also spawns. A
component is the subsystem *above* it, owning one or more tasks; a task failing
is the event that flips its component's health.

```rust
use zaino_async::{Task, TaskName};

# async fn example() {
let task = Task::spawn(TaskName("prune"), |cancel| async move {
    cancel.cancelled().await;
});
task.cancel();
let _ = task.join().await;
# }
```

## Names are typed

[`ComponentName`] and `zaino_async::TaskName` are distinct newtypes. A component
and the tasks it runs are different subjects, so passing one where the other
belongs is a compile error rather than a mislabelled log line.

[`zaino-async`]: https://docs.rs/zaino-async

## Related

- The lifecycle state machine — which transitions are legal, and why a restart
  is a full lap — is specified on the [`Lifecycle`] type.

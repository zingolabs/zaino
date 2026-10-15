---
default: minor
---

# `zaino-primitives`: `serde_secs` adapters for whole-second config keys

`serde_secs::{any, nonzero, zero_is_never}` are `#[serde(with = …)]` adapters between a `Duration` (or `Option<Duration>`, `0` = `None`) and a whole-second integer key, shared by every crate whose config type is a zainod table.

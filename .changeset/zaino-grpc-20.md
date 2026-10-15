---
default: major
---

# `zaino-grpc`: `GrpcLimits` is zainod's `[grpc]` table; the operator hold covers every connection

`GrpcLimits` derives `Deserialize`/`Serialize` with zainod's keys (`*_secs` whole seconds, `0` = never for `max_connection_age_secs` / `max_connection_idle_secs`, `drain_timeout` = `shutdown_grace_secs`, unknown keys refused) and gains `trusted_proxies: TrustedProxies`; it is no longer `Copy`, `GrpcLimits::DEFAULT` is gone (use `Default`) and `GrpcService::with_trusted_proxies` is removed. `Retire` is now a hold, not a one-shot: `Retire::hold(delay)` GOAWAYs every connection once `delay` has passed, including connections accepted later while held; `Retire::resume()` lifts it and `Retire::state()` returns `Hold::{Off, Draining, Drained}`. `Retire::retire_all` and `GrpcService::retire` are removed: the owner passes its `Retire` in with `GrpcService::with_retire`, so a hold can start before the server exists.

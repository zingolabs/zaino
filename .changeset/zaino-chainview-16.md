---
default: major
---

# `zaino-chainview`: `SubmitPolicy` is zainod's `[submission]` table; unreachable `ConfigError` variants removed

`SubmitPolicy` derives `Deserialize`/`Serialize` (`propagation_threshold_secs`, non-zero whole seconds, and `max_attempts`; unknown keys refused) and `SubmitPolicy::DEFAULT` is gone (use `Default`). `ConfigError::NoTrustedValidators` and `ConfigError::TooManyTrustedValidators` are removed: the balancer already refuses an empty or over-64 member list, so `ChainView::new` reports either as `AddressCountMismatch`.

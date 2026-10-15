---
default: major
---

# `zaino-chainview`, `zaino-traffic`: trusted validators named as such

`ChainViewSnapshot::endpoints()` → `trusted_validators()` (test builder `endpoint(..)` → `trusted_validator(..)`), `ConfigError::NoEndpoints` / `TooManyEndpoints` → `NoTrustedValidators` / `TooManyTrustedValidators`. In `zaino-traffic`, the configured-validator member is `TrustedValidator<S>` (was `Trusted<S>`) and `MemberId::TrustedValidator` (was `MemberId::Trusted`); `HeaderAsk::Pinned`'s field is `validator`. Log lines about a trusted validator carry `validator = <address>` (was `endpoint`); metric labels are unchanged.

**Migration:** rename the types, variant, field and method above.

---
default: major
---

# `zainod`: `[[trusted_validators]] max_requests_per_sec` is removed (a header batch was charged one token per header, …

`[[trusted_validators]] max_requests_per_sec` is removed (a header batch was charged one token per header, stalling every request class on that validator), and a config naming it is refused as an unknown field. `max_connections` (at least 5, was 6: one for polling, one reserved per lane + one shared) and `max_mib_per_sec` bound a validator's load.

**Migration:** Remove the `max_requests_per_sec` key from every `[[trusted_validators]]` entry.

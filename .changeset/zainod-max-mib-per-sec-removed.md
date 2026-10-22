---
default: major
---

# `zainod`: `[[trusted_validators]] max_mib_per_sec` removed

Response byte pacing is gone (`max_connections` bounds a validator's load). The key is now an
unknown field: a config still setting it fails to load until the line is deleted.
`zaino_source::LinkLimits` loses `max_bytes_per_sec`.

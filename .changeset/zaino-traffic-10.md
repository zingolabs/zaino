---
default: minor
---

# `zaino-traffic`: `Limits` parses as its `max_connections` and has a default

`Limits` (de)serializes as the bare `max_connections` number, refused at parse below `MIN_CONNECTIONS`; `Limits::default()` = 32 and `Limits::max_connections()` reads it back.

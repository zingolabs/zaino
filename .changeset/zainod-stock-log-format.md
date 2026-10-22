---
default: major
---

# `zainod`: stock `tracing-subscriber` log format

- **Terminal:** the custom aligned layout is gone for `tracing-subscriber`'s default `fmt` line:
  `2026-09-28T17:29:42.659204Z  INFO component{component=ZainoNFS}: zainod::telemetry::progress: Syncing blocks height=3503023 …`.
  No digit grouping, column padding or hash shortening; an absent height is left out instead of
  `—`; an error's causes follow as `error.sources=[…]`.
- **JSON:** heights are numbers (were grouped strings), absent ones left out. One component per
  line (component spans are roots).
- Messages, field names and the `ZAINOLOG_*` / `RUST_LOG` variables are unchanged.

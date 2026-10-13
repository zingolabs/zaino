---
default: major
---

# `zainod`: Logs are one line per event: level, `[MM-DD|HH:MM:SS.mmm]` UTC, a component column, the message, then logfmt …

Logs are one line per event: level, `[MM-DD|HH:MM:SS.mmm]` UTC, a component column, the message, then logfmt fields. `ZAINOLOG_FORMAT` is `terminal` (default) or `json`; `stream` and `tree` are removed. `ZAINOLOG_COLOR` defaults to `auto`, and an unrecognised `ZAINOLOG_*` value or a malformed `RUST_LOG` fails startup. While syncing, each index logs `Syncing` every 30 s and `Serving` once synced.

**Migration:** Set `ZAINOLOG_FORMAT=terminal` (or `json`) where it was `stream` or `tree`.

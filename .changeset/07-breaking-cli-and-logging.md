---
default: major
---

# `zainod`: command line and log format

Commands: `start`, `generate-config [-o FILE]`, `verify`, `hold` and `resume`. `-c, --config FILE`
is accepted before or after any command (default `$XDG_CONFIG_HOME/zaino/zainod.toml`), and
`generate-config` without `-o` writes to that path.

Logs are logfmt on stdout, one line per event (`ts=… level=… target=… message=… key=value …
component=…`), coloured only on a terminal. `RUST_LOG` filters (unset = `zaino=info,zainod=info`);
a malformed one fails startup.

**Migration:** `ZAINOLOG_FORMAT`, `ZAINOLOG_COLOR` and `ZAINOLOG_LOCATION` are gone (JSON output
included): unset them and parse logfmt.

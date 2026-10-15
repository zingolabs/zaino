---
default: major
---

# `zainod`: command line and log format

Commands: `start`, `generate-config [-o FILE]`, `verify`, `hold` and `resume`. `-c, --config FILE`
is accepted before or after any command (default `$XDG_CONFIG_HOME/zaino/zainod.toml`), and
`generate-config` without `-o` writes to that path.

Logs are one line per event: level, `[MM-DD|HH:MM:SS.mmm]` UTC, a component column, the message,
then logfmt fields.

| Variable | 0.11 values |
| --- | --- |
| `ZAINOLOG_FORMAT` | `terminal` (default) or `json`. `stream` and `tree` are gone |
| `ZAINOLOG_COLOR` | `true`, `false` or `auto` (default `auto`, was on) |
| `ZAINOLOG_LOCATION` | `true` or `false` (default `false`): source location per line |
| `RUST_LOG` | filter; unset = `zaino=info,zainod=info` |

An unrecognised `ZAINOLOG_*` value or a malformed `RUST_LOG` fails startup instead of falling
back to a default.

**Migration:** set `ZAINOLOG_FORMAT=terminal` (or `json`) where it was `stream` or `tree`.

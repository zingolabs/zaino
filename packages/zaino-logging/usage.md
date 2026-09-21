# zaino-logging

Centralised `tracing` setup for Zaino, plus a panic hook that routes every panic
through the same sink as every other error. One layer of cross-cutting infra,
depending only on the tracing stack — a consumer that just wants structured logs
need not pull a heavier "common" grab-bag (and its transitive `zebra-chain`) to
initialise them.

## Initialising

```rust,no_run
// Configured entirely from the environment (see below).
zaino_logging::init();
```

`init` installs the global subscriber and the panic hook, panicking if a
subscriber is already set. `try_init` is the idempotent variant for tests, where
several test functions may each try to initialise.

## Configuration

Read from the environment when logging is initialised:

| Variable | Effect |
|---|---|
| `RUST_LOG` | Standard tracing filter. Unset, only zaino crates log; `RUST_LOG=info` includes every crate, or filter explicitly (`RUST_LOG=zaino=debug,zebra_state=info`). |
| `ZAINOLOG_FORMAT` | `stream` (flat chronological, default), `tree` (span nesting), or `json` (machine-parseable). |
| `ZAINOLOG_COLOR` | `true`/`false` to force ANSI colour, `auto` to detect a TTY. Colour on by default. |
| `ZAINOLOG_LOCATION` | Show each event's source `file:line`. Off by default — opt in when debugging. |

## Panic-at-origin

The panic hook logs each panic as a structured `error` event at its origin —
thread, location, and message — so a panic anywhere, including one on a worker
thread that would otherwise surface only as a distant `JoinError::Panic`, is
logged coherently through the same sink as every other error. It is structural:
any binary that initialises logging gets it, with no per-site handling. The hook
forces the `panic` target on so no filter drops the event, and only adds the
log — the panic still unwinds, and the pre-existing hook still runs.

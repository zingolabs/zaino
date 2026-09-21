# Logging Configuration

Zaino provides flexible logging with three output formats and configurable verbosity levels.

## Environment Variables

| Variable | Values | Default | Description |
|----------|--------|---------|-------------|
| `RUST_LOG` | Filter string | `zaino=info,zainod=info,zainodlib=info` | Log level filter (`zaino` matches every `zaino_*` crate) |
| `ZAINOLOG_FORMAT` | `stream`, `tree`, `json` | `stream` | Output format |
| `ZAINOLOG_COLOR` | `true`, `false`, `auto` | `true` | ANSI color output |

## Log Formats

### Stream (default)
Flat chronological output with timestamps. Best for general use and piping to files.
```
14:32:01.234  INFO zainodlib::indexer: Starting Zaino
14:32:01.456  INFO zainodlib::indexer: validator JSON-RPC answering validator=127.0.0.1:8232
```

### Tree
Hierarchical span-based output showing call structure. Best for debugging complex flows.
```
<span>
├─ INFO <event>
└─ <child span>
   └─ INFO <event>
```

### JSON
Machine-parseable output. Best for log aggregation systems (ELK, Loki, etc).
```json
{"timestamp":"2024-01-15T14:32:01.234Z","level":"INFO","target":"zainodlib::indexer","message":"Starting Zaino"}
```

## Usage Examples

### Local Development

```bash
# Default logging (stream format, zaino crates only at INFO level)
zainod start

# Tree format for debugging span hierarchies
ZAINOLOG_FORMAT=tree zainod start

# Debug level for zaino crates
RUST_LOG=zaino=debug,zainod=debug,zainodlib=debug zainod start

# Include dependency crates (tonic, hyper, ...)
RUST_LOG=info zainod start

# Fine-grained control
RUST_LOG="zaino_indexer=debug,zaino_sync=debug,zaino_source_zebra_rpc=info,zaino_lightserve=info,zainodlib=info" zainod start

# Disable colors (for file output)
ZAINOLOG_COLOR=false zainod start 2>&1 | tee zainod.log
```

### Tests

The test environment passes logging variables through:

```bash
# Default (stream format)
cargo nextest run --workspace

# Tree format in tests
ZAINOLOG_FORMAT=tree cargo nextest run --workspace

# Debug logging in tests
RUST_LOG=debug ZAINOLOG_FORMAT=tree cargo nextest run --workspace

# JSON output for parsing test logs
ZAINOLOG_FORMAT=json cargo nextest run --workspace 2>&1 | jq .
```

### Production

```bash
# JSON for log aggregation
ZAINOLOG_FORMAT=json ZAINOLOG_COLOR=false zainod start

# Structured logging to file
ZAINOLOG_FORMAT=json ZAINOLOG_COLOR=false zainod start 2>> /var/log/zainod.json

# Minimal logging
RUST_LOG=warn zainod start
```

# Logging Configuration

Zaino provides flexible logging with three output formats and configurable verbosity levels.

## Environment Variables

| Variable | Values | Default | Description |
|----------|--------|---------|-------------|
| `RUST_LOG` | Filter string | `zaino=info,zainod=info` | Log level filter |
| `ZAINOLOG_FORMAT` | `stream`, `tree`, `json` | `stream` | Output format |
| `ZAINOLOG_COLOR` | `true`, `false`, `auto` | `true` | ANSI color output |

## Log Formats

### Stream (default)
Flat chronological output with timestamps. Best for general use and piping to files.
```
14:32:01.234  INFO zaino_state::indexer: Starting indexer
14:32:01.456  INFO zaino_state::indexer: Connected to validator
```

### Tree
Hierarchical span-based output showing call structure. Best for debugging complex flows.
```
indexer
├─ INFO Starting indexer
└─ validator_connection
   └─ INFO Connected to validator
```

### JSON
Machine-parseable output. Best for log aggregation systems (ELK, Loki, etc).
```json
{"timestamp":"2024-01-15T14:32:01.234Z","level":"INFO","target":"zaino_state::indexer","message":"Starting indexer"}
```

## Usage Examples

### Local Development

```bash
# Default logging (stream format, zaino crates only at INFO level)
zainod start

# Tree format for debugging span hierarchies
ZAINOLOG_FORMAT=tree zainod start

# Debug level for zaino crates
RUST_LOG=zaino=debug,zainod=debug zainod start

# Include zebra logs
RUST_LOG=info zainod start

# Fine-grained control
RUST_LOG="zaino_state=debug,zaino_serve=info,zebra_state=warn" zainod start

# Disable colors (for file output)
ZAINOLOG_COLOR=false zainod start 2>&1 | tee zainod.log
```

### Makefile / Container Tests

The test environment passes logging variables through to containers:

```bash
# Default (stream format)
makers container-test

# Tree format in tests
ZAINOLOG_FORMAT=tree makers container-test

# Debug logging in tests
RUST_LOG=debug ZAINOLOG_FORMAT=tree makers container-test

# JSON output for parsing test logs
ZAINOLOG_FORMAT=json makers container-test 2>&1 | jq .
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

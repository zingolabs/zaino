# Docker Usage

This document covers running Zaino using the official Docker image.

## Overview

The Docker image runs `zainod` - the Zaino indexer daemon. The image:

- Uses `zainod` as the entrypoint with `start` as the default subcommand
- Runs as non-root user (`container_user`, UID 1000) after initial setup
- Handles volume permissions automatically for default paths

For CLI usage details, see the CLI documentation or run `docker run --rm zaino --help`.

## Configuration Options

The container can be configured via:

1. **Environment variables only** - Suitable for simple deployments, but sensitive fields (passwords, secrets, tokens, cookies, private keys) cannot be set via env vars for security reasons
2. **Config file + env vars** - Mount a config file for sensitive fields, override others with env vars
3. **Config file only** - Mount a complete config file

For data persistence, volume mounts are recommended for the database/cache directory.

## Deployment with Docker Compose

The recommended way to run Zaino is with Docker Compose, typically alongside Zebra:

```yaml
services:
  zaino:
    image: zaino:latest
    ports:
      - "8137:8137"   # gRPC
      - "8237:8237"   # JSON-RPC (if enabled)
    volumes:
      - ./config:/app/config:ro
      - zaino-data:/app/data
    environment:
      - ZAINO_VALIDATOR_SETTINGS__VALIDATOR_JSONRPC_LISTEN_ADDRESS=zebra:18232
    depends_on:
      - zebra

  zebra:
    image: zfnd/zebra:latest
    volumes:
      - zebra-data:/home/zebra/.cache/zebra
    # ... zebra configuration

volumes:
  zaino-data:
  zebra-data:
```

If Zebra runs on a different host/network, adjust `VALIDATOR_JSONRPC_LISTEN_ADDRESS` accordingly.

## Initial Setup: Generating Configuration

To generate a config file on your host for customization:

```bash
mkdir -p ./config

docker run --rm -v ./config:/app/config zaino generate-config

# Config is now at ./config/zainod.toml - edit as needed
```

## Container Paths

The container provides simple mount points:

| Purpose | Mount Point |
|---------|-------------|
| Config | `/app/config` |
| Database | `/app/data` |

These are symlinked internally to the XDG paths that Zaino expects.

## Volume Permission Handling

The entrypoint handles permissions automatically:

1. Container starts as root
2. Creates directories and sets ownership to UID 1000
3. Drops privileges and runs `zainod`

This means you can mount volumes without pre-configuring ownership.

### Read-Only Config Mounts

Config files can (and should) be mounted read-only:

```yaml
volumes:
  - ./config:/app/config:ro
```

## Configuration via Environment Variables

Config values can be set via environment variables prefixed with `ZAINO_`, using `__` for nesting:

```yaml
environment:
  - ZAINO_NETWORK=Mainnet
  - ZAINO_VALIDATOR_SETTINGS__VALIDATOR_JSONRPC_LISTEN_ADDRESS=zebra:18232
  - ZAINO_GRPC_SETTINGS__LISTEN_ADDRESS=0.0.0.0:8137
```

### Sensitive Fields

For security, the following fields **cannot** be set via environment variables and must use a config file:

- `*_password` (e.g., `validator_password`)
- `*_secret`
- `*_token`
- `*_cookie`
- `*_private_key`

If you attempt to set these via env vars, Zaino will error on startup.

## Health Check

The image includes a health check:

```bash
docker inspect --format='{{.State.Health.Status}}' <container>
```

## Local Testing

Permission handling can be tested locally:

```bash
./test_environment/test-container-permissions.sh zaino:latest
```

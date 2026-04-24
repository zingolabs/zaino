#!/usr/bin/env bash

# Entrypoint for running Zaino in a container.
#
# The container MUST run as a non-root user. If started as root, the
# entrypoint exits immediately with an error.
#
# Configuration is managed by config-rs using defaults, optional TOML, and
# environment variables prefixed with ZAINO_.
#
# NOTE: This script only handles directories specified via environment variables
# or the defaults below. If you configure custom paths in a TOML config file,
# you are responsible for ensuring those directories exist with appropriate
# permissions before starting the container.

set -eo pipefail

if [[ "$(id -u)" == '0' ]]; then
  echo "ERROR: Refusing to run as root. Run this container as a non-root user." >&2
  exit 1
fi

# Default writable paths.
# The Dockerfile creates symlinks: /app/config -> ~/.config/zaino, /app/data -> ~/.cache/zaino
# So we handle /app/* paths directly for container users.
#
# Database path (symlinked from ~/.cache/zaino)
: "${ZAINO_STORAGE__DATABASE__PATH:=/app/data}"
#
# Cookie dir (runtime, ephemeral)
: "${ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR:=${XDG_RUNTIME_DIR:-/tmp}/zaino}"
#
# Config directory (symlinked from ~/.config/zaino)
ZAINO_CONFIG_DIR="/app/config"

# Create directories if they don't exist
for dir in "${ZAINO_STORAGE__DATABASE__PATH}" "${ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR}" "${ZAINO_CONFIG_DIR}"; do
  [[ -z "${dir}" ]] && continue
  if ! mkdir -p "${dir}" 2>/dev/null; then
    echo "WARN: Cannot create ${dir} (read-only or permission denied), skipping" >&2
  fi
done

exec zainod "$@"

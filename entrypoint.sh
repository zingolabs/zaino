#!/usr/bin/env bash

# Entrypoint for running Zaino in Docker.
#
# This script handles privilege dropping and directory setup for default paths.
# Configuration is managed by config-rs using defaults, optional TOML, and
# environment variables prefixed with ZAINO_.
#
# NOTE: This script only handles directories specified via environment variables
# or the defaults below. If you configure custom paths in a TOML config file,
# you are responsible for ensuring those directories exist with appropriate
# permissions before starting the container.

set -eo pipefail

# Default writable paths.
# The Dockerfile creates symlinks: /app/config -> ~/.config/zaino, /app/data -> ~/.cache/zaino
# So we handle /app/* paths directly for Docker users.
#
# Database path (symlinked from ~/.cache/zaino)
: "${ZAINO_STORAGE__DATABASE__PATH:=/app/data}"
#
# Cookie dir (runtime, ephemeral)
: "${ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR:=${XDG_RUNTIME_DIR:-/tmp}/zaino}"
#
# Config directory (symlinked from ~/.config/zaino)
ZAINO_CONFIG_DIR="/app/config"

# Drop privileges and execute command as non-root user
exec_as_user() {
  user=$(id -u)
  if [[ ${user} == '0' ]]; then
    exec setpriv --reuid="${UID}" --regid="${GID}" --init-groups "$@"
  else
    exec "$@"
  fi
}

exit_error() {
  echo "ERROR: $1" >&2
  exit 1
}

# Creates a directory if it doesn't exist and sets ownership to UID:GID.
# Gracefully handles read-only mounts by skipping chown if it fails.
create_owned_directory() {
  local dir="$1"
  [[ -z ${dir} ]] && return

  # Try to create directory; skip if read-only
  if ! mkdir -p "${dir}" 2>/dev/null; then
    echo "WARN: Cannot create ${dir} (read-only or permission denied), skipping"
    return 0
  fi

  # Try to set ownership; skip if read-only
  if ! chown -R "${UID}:${GID}" "${dir}" 2>/dev/null; then
    echo "WARN: Cannot chown ${dir} (read-only?), skipping"
    return 0
  fi

  # Set ownership on parent if it's not root or home
  local parent_dir
  parent_dir="$(dirname "${dir}")"
  if [[ "${parent_dir}" != "/" && "${parent_dir}" != "${HOME}" ]]; then
    chown "${UID}:${GID}" "${parent_dir}" 2>/dev/null || true
  fi
}

# Create and set ownership on writable directories
create_owned_directory "${ZAINO_STORAGE__DATABASE__PATH}"
create_owned_directory "${ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR}"
create_owned_directory "${ZAINO_CONFIG_DIR}"

# Execute zainod with dropped privileges
exec_as_user zainod "$@"

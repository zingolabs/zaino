#!/usr/bin/env bash

# ------- HELPERS ------------

info() {
  echo -e "\033[1;36m\033[1m>>> $1\033[0m"
}

warn() {
  echo -e "\033[1;33m\033[1m>>> $1\033[0m"
}

err() {
  echo -e "\033[1;31m\033[1m>>> $1\033[0m"
}

is_tag() {
  [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

resolve_build_target() {
  local zebra="$1"

  if is_tag "$zebra"; then
    echo "final-prebuilt"
  else
    echo "final-zebrad-source"
  fi
}


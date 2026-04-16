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
  local zcash="$1"
  local zebra="$2"

  if is_tag "$zcash" && is_tag "$zebra"; then
    echo "final-prebuilt"
  elif ! is_tag "$zcash" && is_tag "$zebra"; then
    echo "final-zcashd-source"
  elif is_tag "$zcash" && ! is_tag "$zebra"; then
    echo "final-zebrad-source"
  else
    echo "final-all-source"
  fi
}


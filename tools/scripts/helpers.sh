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


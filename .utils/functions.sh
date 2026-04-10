#!/usr/bin/env bash
# Shared utility functions for build scripts

get_container_hash() {
  local git_root
  git_root=$(git rev-parse --show-toplevel)
  cd "$git_root"
  git ls-tree HEAD test_environment | git hash-object --stdin | cut -c1-14
}
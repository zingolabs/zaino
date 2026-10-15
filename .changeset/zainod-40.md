---
default: major
---

# `zainod`: The `[snapshot]` bootstrap is always built; the `snapshot` cargo feature is gone

The `snapshot` cargo feature is removed: every build supports `[snapshot]` (it still needs `aria2c` on `PATH` at runtime). The Docker image always installs `aria2c` and drops the `CARGO_FEATURES` build argument; the Nix package drops its `features` argument and puts `aria2c` on `zainod`'s `PATH`.

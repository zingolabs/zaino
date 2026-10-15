---
default: major
---

# `zainod`: `zainodlib` is gone; `zainod` is a binary-only package

The `zainodlib` library target is removed (nothing outside the package used it). `zainod` is a single binary crate with every module private, so the compiler's dead-code lint covers all of it.

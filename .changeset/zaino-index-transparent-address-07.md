---
default: patch
---

# `zaino-index-transparent-address`: `receives` values are stored little-endian

A `receives` row's value (an amount) is stored little-endian, as the protocol encodes amounts; keys and heights stay big-endian.

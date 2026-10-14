---
default: patch
---

# `zaino-internal-value-balance`: output values and fees are stored little-endian

`outputs` values and `fees` records store amounts little-endian, as the protocol encodes them; outpoint keys stay big-endian.

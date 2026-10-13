---
default: minor
---

# `zainod`: `zainod verify` checks every file each enabled index (and the header chain) has committed against its page …

`zainod verify` checks every file each enabled index (and the header chain) has committed against its page checksums, read-only and safe beside a running daemon; JSON report on stdout, exit 0 clean, 1 lost or corrupt, 2 unreadable.

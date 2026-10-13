---
default: patch
---

# `zainod`: SIGTERM / SIGINT exits 0 again

SIGTERM / SIGINT exits 0 again. The NFS and the final follower each read header sync's watch, and `run_until_cancelled` polls its future before the token, so header sync's sender dropping on the same cancel surfaced as `header sync stopped publishing the verified chain` and exit 1 (seen after `zainod hold`, 2026-10-13).

---
default: patch
---

# `zainod`: After a restart, `/readyz`, `/statusz` and `READY=1` wait for the served tip to reach the network's tip

After a restart, `/readyz`, `/statusz` and `READY=1` wait for the served tip to reach the network's tip. Header sync anchors `depth` below a trusted validator's claim, and the NFS served that anchor at once, so the node reported `serving` (and systemd `READY=1`) ~1,000 blocks behind while it fetched the window. It now reports `syncing` (unit `block`, `done` = served, `total` = best) until no trusted validator claims past best and the served tip is best.

---
default: patch
---

# `zaino-snapshot`: `synced` no longer opens while a trusted validator claims a tip past best …

`synced` no longer opens while a trusted validator claims a tip past best (`ChainViewSnapshot::claimed_past_best`). Header sync anchors `depth` below a validator's claim at boot, so a restart served that anchor as the best block and `standing` read `Serving` ~1,000 blocks behind; it now reads `Syncing` until the header chain and the served tip reach the network's tip (G4). `Publisher::run` checks its cancel before the NFS's watch (the NFS stops on the same cancel).

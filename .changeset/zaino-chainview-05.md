---
default: patch
---

# `zaino-chainview`: A first sync no longer stalls at the final boundary: finality is vouched in the header chain (each trusted …

A first sync no longer stalls at the final boundary: finality is vouched in the header chain (each trusted header run vouches its last header; claims and `getblockhash` answers on the chain vouch too), so final = best − depth after every batch, whatever the polls do. Header sync backs off a stalled validator alone (no shared 5 s sleep), skips a claim forked below the final tip until it moves, and never fetches above `final + depth + 2,000`. A poll's claim and `getblockhash` answer are folded before its mempool bytes are fetched.

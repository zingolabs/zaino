---
default: patch
---

# `zaino-snapshot`: `GetMempoolStream` no longer sends transactions the served tip already mined

`GetMempoolStream` no longer sends transactions the served tip already mined. A new epoch opened from the chain view's mempool, whose listings trail a block by up to a poll, so a stream opened just after a block replayed that block's transactions as unmined until the next one (seen on mainnet: 14 of 17 in one opening). Each epoch now skips the txids of `served_nonfinalized()`, in its opening and every arrival; G10 checks it after every publish.

---
default: major
---

# `zaino-source`: `SendRawTransactionError::Rejected { code, message }` carries the validator's JSON-RPC code; `code()` / …

`SendRawTransactionError::Rejected { code, message }` carries the validator's JSON-RPC code; `code()` / `message()` read either variant (`Malformed` = `-22`). zebrad's `-1` verdicts on the transaction itself (already in the mempool, already queued, cached rejection, non-standard) are rejections; its other `-1`s stay failures.

**Migration:** Match `Rejected { message, .. }` where `Rejected(message)` was matched; build it with the code the validator would answer.

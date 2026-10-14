---
default: patch
---

# `zaino-persistence`: Merges classify each key's rows without allocating

A background merge tallies each key's value and tombstone rows in two counters instead of building per-key vectors and strings; a contract-breaking duplicate is described only on that error path, with the same `key held N times: …` message. The LSM's refusal of a `Width::Variable` map key or value now names itself as an LSM limit (`no Variable keys in the LSM`).

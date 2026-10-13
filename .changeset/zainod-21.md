---
default: internal
---

# `zainod`: Boot and `zainod verify` open each index store from its crate's `FORMAT` + `TABLES` (one …

Boot and `zainod verify` open each index store from its crate's `FORMAT` + `TABLES` (one `stores::schema(kind, network)`); folds no longer rebuild a schema per block.

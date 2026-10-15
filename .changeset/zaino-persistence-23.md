---
default: minor
---

# `zaino-persistence`: `LsmConfig` deserializable

`LsmConfig` derives `Deserialize` / `Serialize` (unknown keys refused, missing ones defaulted), so zainod's `[sync.lsm_store]` table parses straight into it.

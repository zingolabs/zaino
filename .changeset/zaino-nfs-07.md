---
default: patch
---

# `zaino-nfs`: `IndexGone` names the durable view

`NfsError::IndexGone` now reads "`<index>` writer stopped publishing its durable view" (was "committed view"). Internal: the publication module and type no longer use the bare term "snapshot".

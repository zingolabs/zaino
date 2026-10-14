---
default: patch
---

# `zaino-persistence`: A store poisoned by a failed commit refuses `apply` too

After a failed commit, `DiskStore::apply` panics like `commit` already did, so nothing is buffered onto a store whose only recovery is a reopen.

---
default: patch
---

# `zaino-persistence`: `verify` reports a foreign or older manifest as its `ManifestError`

`PersistenceEngine::verify` on a directory written for another index kind, format or network, or in an older manifest layout, returns `StoreError::Manifest(Kind | Format | Network | Layout …)` instead of an `InvalidData` I/O error.

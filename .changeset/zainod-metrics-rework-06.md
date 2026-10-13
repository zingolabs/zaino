---
default: major
---

# `zaino-chain-store-zainodb`: `FinalisedStateMode::EphemeralRouted` split into `EphemeralSyncing` and `EphemeralMigrating`

`FinalisedStateMode::EphemeralRouted` split into `EphemeralSyncing` and `EphemeralMigrating`.

**Migration:** Match both new variants wherever `EphemeralRouted` was matched.

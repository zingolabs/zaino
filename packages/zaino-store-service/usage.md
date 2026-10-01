# zaino-store-service

The runtime half of the `zaino-store` tandem: it presents the passive
`zaino-store::StoreReader` to the Orchestra as a supervised, owned component,
mirroring `zaino-chain-head-service` for the non-finalised tier. `zaino-store`
itself stays a reader with no component dependency.

## StoreComponent

`StoreComponent` wraps the reader as a supervised component. The reader is
passive — no run-loop — so its lifecycle is `Offline → Spawning → Ready`, ready
as soon as the backend is open, and it never escalates on its own. Boot
ordering (the indexer reaching `Ready` first) is the Orchestra's job.

`reader()` hands back the `StoreReader` the engine and serving layer take
snapshots against.

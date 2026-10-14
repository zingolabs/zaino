---
default: major
---

# Workspace: one vocabulary (Finalized / Durable / Applied, TrustedValidator* / Peer*, Zebra indexer, Grpc/Index snapshots)

Type and method renames across crates: `ValidatorMetadata` → `TrustedValidatorMeta`, `ValidatorId` → `TrustedValidatorId`, `EndpointSet` → `TrustedValidatorSet`, `ValidatorP2pSource` → `PeerSource`, `ZcashGrpcClient` → `ZebraIndexerGrpcClient`, `IndexerTls` / `IndexerWatch` → `ZebraIndexerTls` / `ZebraIndexerWatch`, `FinalFollower` → `FinalizedFollower`, `Step::Finalized` → `Step::FinalizedTip` (`Run::finalized_tip`), `Store::committed()` / `staged()` → `durable()` / `applied()` (`DurableView`, `AppliedView`), `zaino_snapshot::Snapshot` → `GrpcSnapshot`, zainod `SnapshotConfig` → `IndexSnapshotConfig`. Config keys and metric names are unchanged.

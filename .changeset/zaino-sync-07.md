---
default: major
---

# Workspace: "finalized" is the one spelling for blockchain finality

`zaino-sync`: `IndexerDataSink<T>` → `FinalizedStream<T>` (module `finalized_stream`), testing `FinalStream` → `FinalizedStreamFixture`, `Run::finalized_tip` (bool) → `Run::ends_at_finalized_tip`. Across the header chain, chain view, NFS and `GrpcSnapshot`: `final_tip` → `finalized_tip` (`VerifiedChain`, `HeaderChain`, `Tips`), `final_height` → `finalized_height`, `HeaderViews::verified_final` → `verified_finalized`, `Rejected::BelowFinal` → `BelowFinalized`, `HeaderChain::finals_kept` → `finalized_headers_kept`. The `Chain tip advanced` log carries `finalized_tip` (was `finalized`). Config keys (`finalised_depth`) and metric names are unchanged.

**Migration:** rename the types, fields and methods above.

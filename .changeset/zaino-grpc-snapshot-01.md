---
default: major
---

# `zaino-grpc-snapshot`: the crate formerly `zaino-snapshot`

`zaino-snapshot` is renamed `zaino-grpc-snapshot` (import path `zaino_grpc_snapshot`), so "snapshot" alone never names what gRPC serves (`GrpcSnapshot`), an index archive (`IndexSnapshot`) or the chain view's state (`ChainViewSnapshot`). `zaino_grpc::Routes::snapshots` is now `grpc_snapshots`. The `[snapshot]` config key is unchanged.

**Migration:** depend on `zaino-grpc-snapshot`, import `zaino_grpc_snapshot::…`, and set `Routes { grpc_snapshots, .. }`.

# zaino-indexer

The index writer, wired as a runtime component. `SourceSyncDriver` adapts the
sync engine ([`zaino-sync`](../zaino-sync/usage.md)) to the component `RunLoop`
seam, so the Orchestra boots and supervises index-building like any other
component (Syncing → Ready, escalate on failure). `SourceProvisioner` is the
supply half: it fetches each height from a validator and projects it into the
engine's context. Both are generic over the source — bound only on the
[`zaino-source`](../zaino-source/usage.md) capability traits they need — so any
validator adapter plugs in, and generic over the fetch strategy (`FullBlocks` or
the cheaper `CompactBlocks`), which changes only what is fetched, not what is
built.

The resume constructors (`resuming`, `resuming_compact`) read the backend's
watermark and start just after it, so a restart continues rather than reindexing
from genesis. The driver reports committed-watermark progress on a one-second
tick and, after the initial catch-up, follows the tip through the source's
subscription; it only ever builds the append-only finalised range, so the
volatile window above the boundary is the chain head's concern.

## Bulk catch-up

The initial catch-up is where the scattered-key indexes pay for random B-tree
inserts, so the driver brackets it in the backend's bulk mode: `begin_bulk`
before the catch-up, `finish_bulk` after it and *before* reporting Ready. The
deferred namespaces read as not-yet-serviceable until the merge completes, so
finishing before Ready keeps the component from claiming readiness over an
incomplete store. Steady-state tip following never enters bulk mode.

`DeferralPolicy` (`Auto` the default, or `Off`) is the deployment's choice of
whether to defer at all; a composition root sets it from config with
`with_deferral`. Under `Auto`, bulk mode is entered only once the catch-up gap
reaches `DEFER_THRESHOLD_BLOCKS` (50,000) — below it a run-log round trip costs
more than it saves. `Off`, or a gap below the threshold, keeps the direct write
path unchanged. A bulk load a previous run left unfinished (reported by the
backend's `bulk_pending`) is always re-entered and completed regardless of the
policy or the remaining gap, since leaving a namespace unreadable is never a
policy. How the backend defers, and its crash and disk behaviour, are in
[`zaino-backend-lmdb`](../zaino-backend-lmdb/usage.md); the daemon's knob is in
[`zainod`](../zainod/usage.md).

## Errors

`IndexerError` keeps each cause typed. The source-facing axes stay distinct —
`SourceUnreachable` (the resilient source's retry ladder is spent), `Transport`,
`Domain` — from the internal-fault channel `UnexpectedWorkerFailure` (a spawned
worker panicked or was aborted, named by our task name, not tokio's id).
`Bulk`/`BulkProbe` carry a failure to bracket or probe the catch-up. The
provisioner never re-implements retry: it reacts to a typed `SourceError`.

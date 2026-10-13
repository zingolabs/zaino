---
default: minor
---

# `zaino-nfs`: First release

First release. The tip overlay: `Nfs::new(chain, balancer, params, depth, lookahead)` follows the header chain's `VerifiedChain` and, while the lowest durable tip of its indexes (`add(kind, IndexHandle)`) is within `2 · depth` of best and the header above it is held (a chain anchored above it = the final path fetches the gap by height), folds every best-chain block above it in RAM, each through every index durable below it, in dependency order. It never writes and never sends to the writers (final blocks reach them through `zaino_sync::FinalFollower`). `indexed()` publishes one `Indexed` per served-tip or durable-tip move: `served()` (the deepest folded best block, else the lowest durable tip; a reorg moves it at once), `at(hash)` for any folded block (side branches included) or the root, `Views` with a typed reader per enabled index (`None` = disabled), `durable()`. `run(cancel)` stops on `NfsError::{Fold, ChainGone, IndexGone}`. Metrics and logs for the served tip and reorgs (`describe_metrics`). Feature `testing`: `Indexed::fixed`, `ChainParams::of`.

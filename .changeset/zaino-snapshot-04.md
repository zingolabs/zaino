---
default: major
---

# `zaino-snapshot`: `Snapshot::standing(now_unix)` (`Serving`, `Degraded(Down)`, `Syncing`, `Down(Down)`) replaces `unready()` / …

`Snapshot::standing(now_unix)` (`Serving`, `Degraded(Down)`, `Syncing`, `Down(Down)`) replaces `unready()` / `Unready`: no trusted validator vouching (`Down::NoValidatorReachable`: every one down; `Down::TipNotHeld`) is `Degraded` while synced to a best whose block time is within 30 min of `now_unix` (peers carried the tip), else `Down`; `Syncing` also while every holder of best is catching up. `Snapshot::sync_target()` = best, or a catching-up validator's clock estimate when higher. `notes(&snap)` = one sentence per raised alarm. `Report` drops `unready`. `Snapshot::lightd()` returns `Lightd { served, best, schedule }` from the snapshot alone (no validator), so it answers while degraded.

**Migration:** `snap.unready()` → `snap.standing(now_unix)`; `Down::detail()` for the sentence; render `GetLightdInfo` from `Lightd` (branch and next upgrade from `schedule` at `served`).

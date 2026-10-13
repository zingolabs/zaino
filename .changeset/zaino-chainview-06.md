---
default: major
---

# `zaino-chainview`: `held_by()` = answering validators (`Live` or `CatchingUp`) whose claim is the best or above it (a trusted …

`held_by()` = answering validators (`Live` or `CatchingUp`) whose claim is the best or above it (a trusted validator holding a block holds every block below it). Each poll asks one `getblockhash`, at the view's best as the poll starts (no final-boundary item). `Agreement::Ahead` = claims higher, its answer on the verified chain. `Alarms::finality_paused` now = a block `depth` deep owed finality for 60 s with the final tip unmoved (was: the boundary unheld at that instant). The `Holders` core (and its per-poll stamps) is gone.

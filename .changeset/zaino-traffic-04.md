---
default: major
---

# `zaino-traffic`: `ask_each_poll(heights)` is replaced by `poll_best(f)`: each poll asks `getblockhash` at `f()`, read as the …

`ask_each_poll(heights)` is replaced by `poll_best(f)`: each poll asks `getblockhash` at `f()`, read as the poll starts, and a new best wakes no poll. `Observation.asked` is `Option<Height>`.

**Migration:** Pass a closure returning the current best height to `poll_best` once (`ChainView::new` does).

---
default: minor
---

# `zaino-sync`: `lookahead` = blocks in flight on both fetch paths; a short batch is a misanswer

`FinalFollower`'s `lookahead` now counts blocks in flight by hash and by height alike (the by-hash path held a quarter of it). A by-height reply short of its heights is `Misanswer::Count`: the sender is reported and the batch asked again, never truncated.

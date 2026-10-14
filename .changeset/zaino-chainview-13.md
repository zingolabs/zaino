---
default: major
---

# `zaino-chainview`: `ChainView::new` takes the shutdown token; submissions end with it

`ChainView::new(addresses, balancer, depth, schedule, shutdown)`: a submission still relaying when `shutdown` is cancelled stops, and its caller reads "submission ended" instead of the relay outliving the daemon.

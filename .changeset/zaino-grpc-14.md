---
default: major
---

# `zaino-grpc`: `GrpcService::new(bind, limits)` takes no routes: a server binds and runs before its owner has a chain, …

`GrpcService::new(bind, limits)` takes no routes: a server binds and runs before its owner has a chain, answering health and every method `UNAVAILABLE` (`starting: not routed yet`) until `routing()` → `Routing::route(routes)` (once; twice panics). `with_routes(routes)` routes it from the start.

**Migration:** `GrpcService::new(routes, bind, limits)` → `GrpcService::new(bind, limits).with_routes(routes)`.

---
default: minor
---

# `zaino-grpc`: Public health on the gRPC listener: `GrpcService::with_health(Arc<dyn Health>)` answers `GET /readyz` and …

Public health on the gRPC listener: `GrpcService::with_health(Arc<dyn Health>)` answers `GET /readyz` and `GET /status` over HTTP/1.1 or h2, before admission (never a work permit), 10 at once per connection then one per second (`429` past it). TLS offers ALPN `h2` and `http/1.1`. `LoadReport::overloaded` = ≥ 10% of work refused (≥ 1/s) for 5 min, cleared after 5 min under 2%. `lightd_info(snap, network)` and `LightdInfo` exported.

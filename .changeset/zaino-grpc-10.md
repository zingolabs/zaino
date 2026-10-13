---
default: patch
---

# `zaino-grpc`: The client's `grpc-timeout` is honoured as its deadline: a call still pending answers `DEADLINE_EXCEEDED`, a …

The client's `grpc-timeout` is honoured as its deadline: a call still pending answers `DEADLINE_EXCEEDED`, a stream ends with it in the trailers. Streams have no total-duration cap. `GrpcLimits::stall_timeout` now bounds no progress on either side: unread data closes the connection as before, and a handler or stream with nothing to send for that long ends `UNAVAILABLE` (`GetMempoolStream` exempt).

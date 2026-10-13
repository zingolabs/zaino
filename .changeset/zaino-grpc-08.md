---
default: patch
---

# `zaino-grpc`: `GetBlockRange` (and the deprecated `GetBlockRangeNullifiers`) reaching past the served tip streams every …

`GetBlockRange` (and the deprecated `GetBlockRangeNullifiers`) reaching past the served tip streams every block up to the tip, then ends `OUT_OF_RANGE` in the trailers, as lightwalletd does; it used to end `OK` at the tip, which hangs the iOS downloader. A range whose first height is past the tip is `OUT_OF_RANGE` before any block.

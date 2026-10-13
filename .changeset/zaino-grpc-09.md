---
default: patch
---

# `zaino-grpc`: Streamed answers join records that are ready back to back into full 16 KiB DATA frames instead of one frame …

Streamed answers join records that are ready back to back into full 16 KiB DATA frames instead of one frame per record. `GetAddressUtxosStream`, `GetMempoolTx` and `GetBlockRange`'s in-memory blocks used to send one small frame each, and an h2 0.4.16+ client that reads late answered `GOAWAY too_many_data_frames` (lightwalletd #593). A record already 16 KiB or larger is sent as is; nothing waits for more data.

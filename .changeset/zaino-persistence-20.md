---
default: patch
---

# `zaino-persistence`: The `Barrier` durability probe checks each sequence's `.crc` file

The open-time probe that picks `Barrier` commits (one device flush for every written-back file) now checks the `.crc` checksum files a commit writes back as well as the `.dat` / `.idx` files, so a checksum file on another device, on DAX or sharing extents keeps the store on per-file fsyncs.

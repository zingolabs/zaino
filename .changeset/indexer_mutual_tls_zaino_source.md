---
default: major
---

# `zaino-source`: mutual TLS to a validator's indexer gRPC

`ZcashGrpcClient::at` and `IndexerWatch::at` take `Option<&IndexerTls>` (`ca`, `cert`, `key`, optional `server_name`, default the address's host): `None` = plaintext as before, `Some` = mutual TLS, which zakurad requires on a non-loopback indexer address. The PEM files are read in `at`; an unreadable one is the new `EndpointError::TlsFile`, naming it.

**Migration:** pass `None` for the previous plaintext behaviour.

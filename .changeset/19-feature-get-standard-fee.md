---
default: minor
---

# `zainod`: `GetStandardFee` over gRPC

`GetStandardFee` (lightwallet-protocol v0.6.0) answers `StandardFee { feeZat, estimatorVersion }`:
the ZIP 317 marginal fee in zatoshis per logical action, from a trusted validator's
`getstandardfee`. When every validator asked predates that RPC it answers `UNIMPLEMENTED`.

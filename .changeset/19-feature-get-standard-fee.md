---
default: minor
---

# `zainod`: `GetStandardFee` over gRPC

`GetStandardFee` (lightwallet-protocol v0.6.0) answers `StandardFee { feeZat, estimatorVersion }`:
the ZIP 317 marginal fee in zatoshis per logical action for the block after the best tip, with
estimator version 0. zainod computes it from the network and that height, as zebrad's
`getstandardfee` does (5,000 on mainnet below 3,590,000, then 1,000; 1,000 on test networks), so it
answers with no validator reachable. With no header chain yet it answers `UNAVAILABLE`.

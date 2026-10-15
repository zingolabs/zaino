---
default: minor
---

# `zaino-source`: `ChainDataSource::get_standard_fee`

`ChainDataSource::get_standard_fee` (with `StandardFee` and `GetStandardFeeError`): the ZIP 317 marginal fee, zatoshis per logical action, from the validator's `getstandardfee` (ZIP 2009 lowers it from 5000 to 1000). A validator that predates the RPC answers method-not-found, which classifies as `Unsupported`, a domain answer. A zero fee reply fails at parse.

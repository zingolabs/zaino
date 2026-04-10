# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]


### Added
- `ValidatedBlockRangeRequest` type that encapsulates validations of the
  `GetBlockRange` RPC request
- utils submodule to handle `PoolType` conversions
- `PoolTypeError` defines conversion errors between i32 and known `PoolType` variants
- `PoolTypeFilter` indicates which pools need to be returned in a compact block.

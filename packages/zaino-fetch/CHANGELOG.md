# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased
- [943] Zallet regtest fixes

### Added 
### Changed
- `JsonRpSeeConnector::get_tree_state` now returns a `GetTreestateResponse`
  whose `sapling` and `orchard` fields are optional. In regtest mode, these
  fields may be omitted when the corresponding network upgrade activation
  height is not configured.
### Removed
### Deprecated

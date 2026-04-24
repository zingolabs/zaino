#!/usr/bin/env bash
# Run the integration tests that exercise the `getblockhash` feature,
# plus one wallet_to_validator control test.
#
# - fetch_service::{zcashd,zebrad}::get::blockhash              green
# - state_service::zebra::get::blockhash_regtest                green (happy path)
# - state_service::zebra::get::blockhash_out_of_range_returns_err  red until the
#   StateService `.unwrap()`/`todo!()` bug (item 1) is fixed
# - wallet_to_validator::zcashd::connect_to_node_get_info       control

set -euo pipefail

# integration-tests is a separate cargo workspace from the root, so nextest
# needs --manifest-path to see the fetch_service/state_service/wallet_to_validator
# test binaries (matching the convention in Makefile.toml's $ITESTS).
exec makers container-test \
    --manifest-path integration-tests/Cargo.toml \
    -E '(binary(fetch_service) & test(=zcashd::get::blockhash)) | (binary(fetch_service) & test(=zebrad::get::blockhash)) | (binary(state_service) & test(=zebra::get::blockhash_regtest)) | (binary(state_service) & test(=zebra::get::blockhash_out_of_range_returns_err)) | (binary(wallet_to_validator) & test(=zcashd::connect_to_node_get_info))' \
    "$@"

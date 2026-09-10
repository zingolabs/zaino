# `client_rpc_test_fixtures` moves to its own repository

## Status

accepted

## Context and decision

zingolib depends on infra crates (for testing): zcash_local_net and zingo_test_vectors.
client_rpc_test_fixtures depends on zingolib.
As such, there is a problem with client_rpc_test_fixtures being in infra:
Imagine we need to update infra, and part of that update involves updating the version of
(for example) zcash_client_backend. In order to update zingolib, we need to have updated
infra for zingolib to depend on. However, in order to update client_rpc_test_fixtures,
we may need to update zingolib. This is a circular problem, we can't update one repo
without first updating the other. As such, client_rpc_test_fixtures have been moved to their
own repo: https://github.com/zingolabs/client_rpc_test_fixtures.

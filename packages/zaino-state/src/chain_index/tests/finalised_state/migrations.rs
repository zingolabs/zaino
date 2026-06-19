//! Holds database migration tests.

mod v0_to_v1;

// These suites build addrhist-free v1.0.0 fixtures (via `write_block_v1_0_0`) and migrate them
// forward. The v1.x migrations rebuild the core `spent` / `txid_location` indices but deliberately
// do not backfill the experimental `address_history` index — that belongs with the proper
// introduction of transparent address history in a later version. Under
// `transparent_address_history_experimental`, post-migration validation would require that
// unbuilt index, so these suites only run with the feature disabled. (v0->v1 is unaffected: its
// shadow rebuild writes blocks through the full `write_block`, which populates addrhist.)
#[cfg(not(feature = "transparent_address_history_experimental"))]
mod v1_0_to_v1_1;
#[cfg(not(feature = "transparent_address_history_experimental"))]
mod v1_1_to_v1_2;

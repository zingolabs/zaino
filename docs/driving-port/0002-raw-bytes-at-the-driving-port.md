# The driving port hands consumers raw consensus-serialized bytes

Zaino's driving port traffics in consensus-serialized bytes: blocks,
transactions, and treestate frontiers cross the boundary as bytes. Only
identifiers and locators — heights, block hashes, txids, consensus branch
ids — are typed, and those types come from `zaino-primitives`
(zingolabs/zaino#1402), a zero-dependency crate of checked domain types. We
chose byte payloads over typed domain payloads because the port's consumers
already parse consensus bytes into the types they actually want: Zallet reads
blocks and transactions into `zcash_primitives` types (zcash/zallet,
`backends/zaino/src/chain.rs`), so a typed payload would only add a
conversion detour.

This deliberately deviates from the typed philosophy of Zaino's driven ports
(zingolabs/zaino#1402: "source traits return typed `Block`, not `Vec<u8>`").
The two boundaries face opposite directions: a driven port feeds Zaino's own
core, which needs structured data, while the driving port feeds external
consumers that own their parsing.

## Considered Options

We rejected typing the payloads with `zaino-primitives`, because consumers
would deserialize Zaino's types only to convert them into their own; the
identifier types are the useful subset, and the port takes exactly those. We
rejected typing the payloads with `zcash_primitives`, because Zaino's engines
would then take on librustzcash version-lockstep with the wallet stack — the
coupling the Z3 stack keeps working to shed.

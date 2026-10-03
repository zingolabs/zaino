# zaino-block-decode

Consensus-encoded block bytes projected straight to the domain `Block`,
without deserialising what the domain block does not keep.

## What it is for

A validator's raw block read carries proofs, signatures, value commitments and
full note ciphertexts. A full deserialiser such as zebra-chain's decompresses
a curve point for every shielded output and action, and on shielded-heavy
blocks that is most of the cost of reading a block the indexer will then
project down to nullifiers, commitments, ephemeral keys and ciphertext heads.

`decode_block(bytes, chain_metadata)` walks the encoding once, keeps the fields
the domain `Block` holds as the bytes they are on the wire, and steps over the
rest by size. `decode_transaction(bytes)` does the same for one transaction on
its own, a mempool transaction say. Every transaction version the chain has
carried is handled: v1 and v2 with JoinSplits, v3, v4, v5 and v6 with its
Ironwood bundle.

```text
decode(bytes) = project ∘ walk                          walk never touches a curve
decode(bytes) == block_from_zebra(deserialise(bytes))   (the oracle)
```

## The transaction id

The id is computed over the same bytes: the double-SHA256 of the encoding for
v1 to v4, and the ZIP-244 digest tree for v5 and v6. Every leaf of that tree
is a BLAKE2b-256 over field bytes exactly as encoded, which is what lets the
walk stay at the byte level and still produce the id a full deserialiser
would. The v6 tree appends the Ironwood bundle digest and uses ZIP-229's
personalisations where they differ.

## What it checks and what it does not

It rejects what would make the bytes not a block: truncation, trailing bytes,
a non-canonical compact-size prefix, an unknown version word, a block without
transactions, a first transaction that is not a coinbase, a coinbase whose
height push is not canonical. These are the shape checks zebra's deserialiser
makes, and nothing more: proofs, signatures and curve points are never
examined. A consumer that needs a validated block has the validator for that.

## The oracle

`tests/oracle.rs` decodes each fixture block both ways, through this crate and
through zebra-chain plus `zaino-convert-zebra`, and asserts equality
transaction by transaction. The fixtures are mainnet blocks chosen for the
formats they carry, from block 1 to a post-Ironwood block with v6
transactions. A change to either side that makes the two disagree fails
there.

## Where it is used

The zebra JSON-RPC source adapter decodes `getblock <h> 0` through it, so the
`full` fetch strategy of an RPC deployment reads blocks from any validator
without paying for fields the indexes never read. The ReadState adapter is
untouched: zebra hands it a deserialised block already.

# Explorer Transaction View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve `getrawtransaction <txid> 1` and `getblock <block> 0|1|2` in the
exact shape the NightHawk explorer reads, with transparent inputs resolved to the
value and address they spend. All of it is passthrough-first.

**Architecture:** The domain `Transaction` is the indexing shape. It drops the
coinbase input, the transaction envelope and the Sprout pool values. A new domain
type `TransactionDetail` carries those facts. The zebra-rpc adapter computes it
from the zebra transaction it has already deserialised, so zaino-core stays
validator-agnostic.

A new driven port decodes a whole block from ONE raw-block fetch. zaino-core
composes the service read `TransactionViewRead`, which resolves every transparent
prevout:
- first from the same block;
- otherwise through the resilient decoded-transaction port, deduplicated and with
  bounded concurrency.

zaino-noderpc renders one shared transaction DTO for both `getrawtransaction` v1
and `getblock` v2.

**Tech Stack:** Rust, zebra-chain (git rev already pinned), zcash_address /
zcash_protocol, serde_json, futures.

**Spec:**
- `zaino-design/design/explorer-grant-completion.md`: the method ledger and slices.
- `zaino-design/design/explorer-tx-contract.md`: per field of the tx object, the
  explorer's read sites, how strictly it reads each one, and the minimum
  non-crashing shape. **This contract is binding.**
- SDD ledger `.superpowers/sdd/2026-10-02-explorer-noderpc-slice-4/progress.md`,
  rulings R37–R45.

## Global Constraints

**Error handling**
- Never blindly stringify errors. Handle each one coherently for its context, keep
  the source chain with `#[source]`, and add a `From` only when the conversion is
  context-free.
- No `unwrap`/`expect` in production code.
- The existing `read_error!` String types (`BlockReadError`, `TxReadError`, …) are
  a known exception. Do not add to them. New error types are typed enums with
  `#[source]`.

**Code conventions**
- No `as` numeric casts. Use `From`/`TryFrom` or checked ops.
- No `mod.rs`; use `foo.rs` + `foo/`. Use the minimum visibility.
- Doc comments describe the present code only.
- Every new public capability gets an entry in its crate's `usage.md`.
- Never use sed or python on Rust source; use the editor tools.

**Architecture boundaries**
- Adapters only adapt.
  - zaino-noderpc = JSON-RPC transport + DTO translation.
  - zaino-source-zebra-rpc = validator wire → domain.
- zaino-core implements the reads.
- zaino-core binds the canonical resilient zaino-source traits, NEVER `OneShot*`.

**Wire shape**
- ZEC floats on the wire come from the single shared zatoshi→ZEC helper introduced
  by the client-compat round (wire.rs). Never compute a float any other way. Exact
  `*Zat` integers travel beside every float.
- Golden JSON tests pin:
  - the exact sorted key set;
  - None as an absent key, never null;
  - a typed value per key;
  - distinguishable non-zero values.

**Gate (every task)**
- `cargo test --all-features` on each of: zaino-primitives, zaino-source,
  zaino-convert-zebra, zaino-source-zebra-rpc, zaino-core, zaino-noderpc,
  zaino-lightserve, zaino-wallet, zaino-runtime, zaino-address, zaino-store.
- `cargo test -p zaino-service --features testing` AND `cargo check -p zaino-service`
  with no features.
- `cargo clippy --no-deps --all-targets --all-features -- -D warnings` on every
  touched crate.
- `cargo fmt`.
- Grep for every constructor of a struct you extend across `packages/` (src AND
  tests), not one crate.

**Process**
- Commit forward and never amend. No Co-Authored-By or any attribution trailer.
- No push, no PR.

## Review Focus

1. **Coinbase transaction fetched alone** (`getrawtransaction` of a coinbase txid).
   It must render `vin: [{coinbase: <hex>, sequence}]` with no block context.
   Coinbase-ness is data, not position (review finding F1 on 5b).
2. **A prevout created earlier in the same block.** It must resolve from the
   block, with no validator call, and a test must prove the call count.
3. **A prevout the validator cannot find.** That is a source inconsistency. It is a
   typed error naming the outpoint, never a silently blank value. An
   out-of-range `prev_index` is likewise its own typed variant.
4. **The genesis coinbase.** `coinbase_script()` special-cases genesis. The hex must
   still be the scriptSig zcashd emits.
5. **v5 transactions with no Orchard actions.** They still emit
   `orchard: {actions: [], valueBalance: 0.0, valueBalanceZat: 0}`, because the
   explorer reads `orchard.valueBalance` strictly on v5. v4 and older emit no
   `orchard` key.

---

### Task 1: `TransactionDetail` — the facts the indexing shape drops

**Files:**
- Create: `packages/zaino-primitives/src/types/transaction_detail.rs` (+ export in `types.rs`)
- Modify: `packages/zaino-convert-zebra/src/lib.rs` (new pub fn + tests)
- Modify: `packages/zaino-source/src/get_transaction_verbose.rs` (`DecodedTransaction` gains `detail`)
- Modify: `packages/zaino-source-zebra-rpc/src/adapter.rs` (`decode_transaction_response` fills it)
- Modify: every `DecodedTransaction { .. }` constructor in `packages/` (the mock builder in zaino-source's testing module, tests in core/service)

**Interfaces:**
- Produces:
  ```rust
  pub struct TransactionDetail {
      pub version: u32,
      pub overwintered: bool,
      pub version_group_id: Option<u32>,
      pub lock_time: u32,                    // raw nLockTime, as zcashd emits `locktime`
      pub expiry_height: Option<Height>,     // None before Overwinter
      pub size: u64,                         // serialized byte length
      pub coinbase: Option<CoinbaseInput>,   // Some iff the tx is a coinbase
      pub joinsplits: Vec<JoinSplitValues>,  // Sprout pool movements, in order
  }
  pub struct CoinbaseInput { pub script: Script, pub sequence: u32 }
  pub struct JoinSplitValues { pub vpub_old: Zatoshis, pub vpub_new: Zatoshis }
  pub fn transaction_detail_from_zebra(tx: &zebra_chain::transaction::Transaction, size: u64)
      -> Result<TransactionDetail, ConvertError>          // in zaino-convert-zebra
  ```
  `DecodedTransaction { transaction, detail: TransactionDetail, location }`.
  All three structs are `Debug, Clone, PartialEq, Eq`.

- [ ] **Step 1: Write the failing tests** in zaino-convert-zebra. Build real
  zebra transactions with zebra's own test vectors where the pinned rev exposes
  them (`zebra_test::vectors`, if it is a dev-dependency already; otherwise build
  `Transaction::V4 { .. }` / `V5 { .. }` values in the test). Cover:
  - a v4 transparent tx: version 4, overwintered, `version_group_id` Some, lock_time
    and expiry passed through, coinbase None, joinsplits empty;
  - a coinbase (`Input::Coinbase`): `coinbase.script` == zebra's
    `coinbase_script()` bytes, plus its sequence. For genesis (height 0), assert the
    genesis scriptSig bytes zcashd emits (read zebra's `GENESIS_COINBASE_SCRIPT_SIG`
    handling and mirror it);
  - a v2/v3 tx with Sprout joinsplits: `vpub_old`/`vpub_new` per joinsplit, in order;
  - a v1 tx: overwintered false, `version_group_id` None, expiry None;
  - `size` is the passed-in length, verbatim.
- [ ] **Step 2: Run them; they fail (fn missing).**
- [ ] **Step 3: Implement** `transaction_detail_from_zebra`. Use `tx.version()`,
  `is_overwintered()`, `version_group_id()`, `raw_lock_time()` and
  `expiry_height()`, the first input's `Input::Coinbase`, and the Sprout joinsplit
  iterators. Amounts are converted via `Zatoshis::new(u64::from(..))`, and a failed
  conversion is mapped to `ConvertError::Value` with the cause preserved.

  `expiry_height`: zebra returns `Some(0)` for "no expiry" on v4/v5. zcashd emits
  `expiryheight: 0` in that case, so keep `Some(Height(0))`. Use `None` only where
  zebra returns `None` (pre-Overwinter).
- [ ] **Step 4: Fill `detail` in `decode_transaction_response`.** `size` is
  `u64::try_from(response.bytes.len())`, mapped through the existing parse failure
  path with its source kept. Update every `DecodedTransaction` constructor (grep
  `packages/`). The mock gets a builder that defaults `detail` to a v5 non-coinbase
  detail, plus `with_detail`.
- [ ] **Step 5: Run the gate. Commit.**

### Task 2: A decoded-block port — one fetch, every transaction with its detail

**Files:**
- Create: `packages/zaino-primitives/src/types/decoded_block.rs`
- Create: `packages/zaino-source/src/get_block_decoded.rs` (+ `lib.rs` export, `usage.md`)
- Modify: `packages/zaino-source/src/testing.rs` (or wherever `MockChain` lives): `OneShot` impls + builder
- Modify: `packages/zaino-source-zebra-rpc/src/adapter.rs`: impl for `ZebraRpcAdapter`
- Modify: every other type implementing the full source composite, if the composite's bound list is extended (grep the composite trait). Prefer NOT extending any composite in this task. `zaino-core`'s `PassthroughProvider` names the new resilient traits directly, as 5b did.

**Interfaces:**
- Produces:
  ```rust
  pub struct DetailedTransaction { pub transaction: Transaction, pub detail: TransactionDetail }
  pub struct DecodedBlock { pub size: u64, pub transactions: Vec<DetailedTransaction> }
  #[resilient_port] pub trait OneShotGetBlockDecoded: ValidatorSource + Send + Sync {
      fn get_block_decoded(&self, height: Height)
          -> impl Future<Output = Result<DecodedBlock, QueryError<GetBlockError, Self::NonDomain>>> + Send;
  }
  #[resilient_port] pub trait OneShotGetBlockDecodedByHash: ValidatorSource + Send + Sync {
      fn get_block_decoded_by_hash(&self, hash: BlockHash)
          -> impl Future<Output = Result<DecodedBlock, QueryError<GetBlockByHashError, Self::NonDomain>>> + Send;
  }
  ```
  These reuse the existing `GetBlockError` / `GetBlockByHashError`: same domain
  question, same misses. The resilient (non-OneShot) names come from the macro.

- [ ] **Step 1: Failing tests.**
  - zebra-rpc adapter: a canned `getblock <h> 0` hex response, using a real
    serialized block. Reuse an existing fixture in the crate's tests if one exists;
    otherwise a zebra test-vector block. Assert:
    - `size` == byte length;
    - the tx count;
    - tx[0].detail.coinbase is Some, and the others are None;
    - the txids equal `transaction_from_zebra`'s.
  - A missing height maps to `GetBlockError::HeightNotFound`, mirroring
    `get_raw_block`.
  - Mock: the builder round-trips.
- [ ] **Step 2: Implement.**
  - The adapter calls `getblock <h|hash> 0` exactly as `get_raw_block` does.
  - It deserialises with zebra.
  - Per transaction, it runs `transaction_from_zebra`, then
    `transaction_detail_from_zebra(tx, size)`. Size comes from
    `zcash_serialized_size()` or an equivalent; no `as`.
  - Parse/convert failures go to `NonDomainError::from_cause(FailureMode::Parse, e)`,
    as `decode_transaction_response` does.
- [ ] **Step 3: Gate. Commit.**

### Task 3: `TransactionViewRead` — prevout resolution in zaino-core

**Files:**
- Modify: `packages/zaino-service/src/reads.rs` (new trait), `read_sets.rs` (add to `NodeRpcReads`), `error.rs` (new typed error), `testing.rs` (service mock), `usage.md`
- Create: `packages/zaino-core/src/engine/transaction_view.rs` (engine impl)
- Create: `packages/zaino-core/src/prevout.rs` (pure resolution logic + its tests)
- Modify: `packages/zaino-core/src/passthrough.rs` (provider methods over the resilient `GetBlockDecoded`/`GetBlockDecodedByHash`/`GetTransactionVerbose`; add the `block_failure` helper that collapses the repeated transport arms, review F2)
- Modify: `packages/zaino-core/src/tests.rs` (`assert_node_rpc` must still compile)

**Interfaces:**
- Produces (zaino-service):
  ```rust
  pub struct ResolvedInput { pub outpoint: TransparentInput, pub spent: TransparentOutput }
  pub struct TransactionView {
      pub transaction: Transaction,
      pub detail: TransactionDetail,
      pub inputs: Vec<ResolvedInput>,          // same order as transaction.transparent.inputs
  }
  pub struct LocatedTransactionView { pub view: TransactionView, pub location: TransactionLocation }
  pub struct BlockTransactionViews { pub size: u64, pub transactions: Vec<TransactionView> }
  pub trait TransactionViewRead: Send + Sync {
      fn transaction_view(&self, id: TransactionId)
          -> impl Future<Output = Result<Option<LocatedTransactionView>, TransactionViewError>> + Send;
      fn block_transaction_views(&self, at: BlockSelector)
          -> impl Future<Output = Result<Option<BlockTransactionViews>, TransactionViewError>> + Send;
  }
  #[derive(Debug, thiserror::Error)]
  pub enum TransactionViewError {
      /// The validator could not be reached or answered unusably.
      #[error("validator unavailable")]
      Unavailable { #[source] cause: Box<dyn std::error::Error + Send + Sync> },
      /// The validator served a spending transaction but not the transaction it spends.
      #[error("prevout {outpoint:?} is unknown to the validator")]
      MissingPrevout { outpoint: TransparentInput },
      /// The spent transaction exists but has no output at that index.
      #[error("prevout {outpoint:?} names output {index} of a transaction with {outputs} outputs")]
      PrevoutIndexOutOfRange { outpoint: TransparentInput, index: u32, outputs: usize },
  }
  ```
  `TransactionViewRead` is always-passthrough, like `BlockVerboseRead`: no
  `Capability` variant and no routing entry (precedent: 5b's report).

- [ ] **Step 1: Failing tests, pure (`prevout.rs`).** The resolution fn takes
  - the block's transactions (or the single tx), and
  - an async fetch closure/trait for the decoded tx.

  Tests:
  - same-block prevout: resolved with ZERO fetches, using a counting fetcher;
  - two inputs spending the same external txid: ONE fetch (dedup);
  - missing prevout → `MissingPrevout` naming the outpoint;
  - index out of range → `PrevoutIndexOutOfRange`;
  - a fetch transport failure → `Unavailable`, with the source chain kept
    (walk `source()`);
  - coinbase tx: no inputs to resolve, no fetch;
  - input order preserved.
- [ ] **Step 2: Implement `prevout.rs`.**
  - Build an intra-block map txid→outputs first.
  - Collect the distinct external txids.
  - Fetch them with `futures::stream::iter(..).map(fetch).buffer_unordered(PREVOUT_FETCH_CONCURRENCY)`,
    where `const PREVOUT_FETCH_CONCURRENCY: usize = 16` is documented as the
    passthrough-phase bound.
  - Then assemble `ResolvedInput`s in input order.
- [ ] **Step 3: Failing engine tests (`tests.rs`)** against
  `ValidatorClient<MockChain>` with `engine_single_attempt`, end to end:
  - `transaction_view` of a coinbase;
  - `transaction_view` of a tx spending a mock-known tx;
  - `block_transaction_views` by height AND by hash. The by-height and by-hash mock
    blocks MUST differ, so swapped selector arms fail (review F3). Do the same for
    the existing 5b `block_verbose` tests: give the two mock ports distinct canned
    values.
  - an unknown txid → `Ok(None)`;
  - an unknown block → `Ok(None)`.
- [ ] **Step 4: Implement the engine impl + provider methods. Add
  `TransactionViewRead` to `NodeRpcReads` and to the service mock.**
  `assert_node_rpc` must still compile with an unchanged bound.
- [ ] **Step 5: Gate. Commit** (pure logic and engine wiring may be two commits).

### Task 4: The shared explorer transaction DTO; `getrawtransaction` v1 and `getblock` 1/2

**Files:**
- Modify: `packages/zaino-address/src/` (new `script.rs`: `transparent_address_from_script`)
- Modify: `packages/zaino-noderpc/src/wire.rs`, `wire/response.rs`, `lib.rs`, `rpc.rs`
- Modify: `packages/zaino-noderpc/usage.md`

**Interfaces:**
- Consumes: `TransactionViewRead` (Task 3), the shared ZEC helper (client-compat
  round), `BlockVerboseRead`/`BlockRead` (5b).
- Produces: `pub fn transparent_address_from_script<P: Parameters>(script: &[u8], params: &P) -> Option<String>`
  in zaino-address. It decodes P2PKH and P2SH to the network's encoding; anything
  else → `None`. Use zcash_transparent / zcash_address types already in the
  dependency tree. Do not hand-roll base58check if a library function exists.

The transaction DTO is built from `zaino-design/design/explorer-tx-contract.md`.
Read the contract's tables and its "Minimum non-crashing shape" section first.
Required keys:
- `txid`, `version`, `overwintered`, `locktime`, `size`; `versiongroupid` (hex
  string) and `expiryheight` when overwintered.
- `vin`:
  - coinbase → `[{coinbase: <scriptSig hex>, sequence}]`;
  - otherwise each input
    `{txid, vout, value (ZEC float), valueSat (zat int), address?}`. `address` is
    absent when the script is not P2PKH/P2SH.
- `vout`: each
  `{value (ZEC float), valueZat, n, scriptPubKey: {hex, addresses: [addr]?, type?}}`.
  `addresses` is a one-element array when decodable, otherwise absent.
  `type` is "pubkeyhash"/"scripthash" when decodable.
  `n` comes from enumerate zipped with a u32 range, not an `.expect` (review F4).
- `vjoinsplit`: each `{vpub_old, vpub_oldZat, vpub_new, vpub_newZat}`.
- `valueBalance` + `valueBalanceZat` when version ≥ 4.
- `vShieldedSpend: [{nullifier}]` and `vShieldedOutput: [{cmu, ephemeralKey}]` when
  version ≥ 4. The explorer reads only the lengths; the per-element keys are zcashd
  spellings.
- `orchard: {actions: [{nullifier, cmx, ephemeralKey}], valueBalance, valueBalanceZat}`
  when version ≥ 5 (Review Focus 5).
- Ironwood is not emitted. That is a recorded divergence, noted in a test comment.
- For `getrawtransaction` v1 only: `height`, `confirmations`, `blockhash`, `time`,
  `blocktime` exactly as the existing v1 location rendering does, or absent for a
  mempool tx.

`getblock`:
- verbosity 1 = the 5b header/verbose keys + `size` + `tx: [txid strings]` +
  `previousblockhash`/`nextblockhash` (when known);
- verbosity 2 = the same with `tx: [transaction DTO]`;
- verbosity 0 is left to Task 5. Until then it is refused with the existing
  message.

`size` now comes from `BlockTransactionViews.size`, which closes divergence R39.
5b's thin `transaction_to_wire` is replaced, not kept beside the new one.

- [ ] **Step 1: Failing tests.**
  - zaino-address: P2PKH and P2SH mainnet + testnet vectors, plus a non-standard
    script → None.
  - noderpc golden key-set tests, one per tx kind: coinbase, transparent, Sapling
    v4, Orchard v5 with actions, v5 without Orchard actions, Sprout v2. Each asserts:
    - the exact key set at tx, vin[i], vout[i], scriptPubKey, joinsplit and orchard
      level;
    - float values from the shared helper next to their exact Zat twins.
  - getblock 1 vs 2 key sets.
  - getrawtransaction v1 for a mempool tx (no location keys).
  - `MissingPrevout` reaches the client as a JSON-RPC error. Choose the code by
    reading `to_error_object`'s existing taxonomy and justify it in a test comment;
    it is a server-side inconsistency, not invalid params.
- [ ] **Step 2: Implement:** DTO structs in `wire/response.rs`, conversion fns in
  `wire.rs`, method bodies in `lib.rs`. Error mapping is per variant, not via
  `to_string()`.
- [ ] **Step 3: Gate. Commit** (zaino-address, then noderpc).

### Task 5: `getblock` verbosity 0

**Files:** `packages/zaino-noderpc/src/{lib.rs,wire.rs}`; zaino-service/zaino-core
only if no existing read already exposes raw block bytes by selector. Check
`BlockRead` and `RawBlockRead`-like reads first. If none exists, add
`raw_block(at: BlockSelector) -> Result<Option<Vec<u8>>, BlockReadError>`, a
passthrough over the resilient `GetRawBlock`/`GetRawBlockByHash`, to the most
fitting existing read trait (not a new one), with engine tests by height and by
hash using distinct mock blocks.

- [ ] **Step 1: Failing test:** `getblock <hash> 0` returns the lowercase hex of the
  raw bytes. An unknown block maps to the same error as verbosity 1/2's unknown
  block. A verbosity outside 0..=2 is refused naming the valid range.
- [ ] **Step 2: Implement. Step 3: Gate. Commit.**

## Self-Review

- Spec coverage. The contract's strict fields per page map as follows:
  - homepage `vout.value` → Task 4;
  - block page `vin.value`/`vout.value`/`mined_by` addresses → Tasks 3+4;
  - block page `getblock 1` → Task 4;
  - tx page coinbase hex, valueBalance, vpub_*, orchard.valueBalance, version →
    Tasks 1+4;
  - search `getblock 0` → Task 5;
  - block `size` (R39) → Tasks 2+4.
- Known divergences, recorded rather than fixed:
  - vin `sequence` for non-coinbase inputs (the domain does not carry it;
    tolerant read);
  - scriptPubKey `asm`/`reqSigs` (tolerant);
  - Ironwood not rendered (unknown to the client);
  - the explorer's `mixed_tx_fees` has no clause for a transparent+Orchard tx
    without Sapling outputs. That is a client bug that zcashd would trigger too, so
    we do not shape our output around it.
- Latency: every `getblock 2` now resolves prevouts by passthrough. The homepage
  warmer fetches 10 blocks every 15 s. The intra-block shortcut and txid dedup bound
  the fan-out, and localising (a local outpoint index) is the "then locally" step.
  Measure on the cluster.

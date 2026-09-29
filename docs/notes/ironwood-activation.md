# Ironwood activation: domain facts

Properties of the network that the live suite and the indexes rely on. The
wallet-observable predicates, and which test covers each in each era, are
tabulated at the top of
[`live-tests/e2e/tests/ironwood_activation.rs`](../../live-tests/e2e/tests/ironwood_activation.rs).

## Facts

- **Ironwood is a new shielded pool activated by NU6.3**
  (<https://zcash.github.io/ironwood/>). It shares the Action-circuit shape with
  Orchard but is a distinct pool with its own note-commitment tree.
- **Activation heights** (zebra-chain 12.0, `parameters/constants.rs`):
  The Public Testnet 4,134,000; Mainnet 3,428,143. Consensus branch ID
  `0x37a5165b`. Explorers can mislabel testnet and mainnet heights; check
  against a synced node.
- **Cross-address restriction**
  (<https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>):
  after NU6.3 the Orchard Action circuit requires "(g_d, pk_d) of the output
  note must equal (g_d, pk_d) of the spent note". Every Orchard action is
  either change to the spent note's own address or a withdrawal (positive value
  balance); a transfer to any other address, including another address of the
  same wallet, is impossible. A companion transaction-level rule forbids new
  value entering the pool.
- **Orchard is not "exit-only".** Same-receiver change still lands in the
  pool, so the Orchard commitment tree keeps growing after activation. There is
  no frozen-final-root predicate; the correct chain-walk predicate is
  "Orchard pool value is non-increasing from the boundary" (observable via
  `valuePools`).
- **ZIP 318, "Orchard to Ironwood migration"** (zcash/zips PR #1317): "The
  user MUST be able to migrate Orchard-pool funds to the Ironwood pool, and
  MUST be informed that doing so is necessary to retain access to those
  funds." The migration transaction is an Orchard spend with Ironwood outputs.
- **The migration is only testable on a regtest net.** The Public Testnet's
  pre-NU6.3 epoch is closed and nothing new can enter Orchard there, so
  pre-activation Orchard notes exist only in wallets that held them before
  the flip. Zaino holds none. The live suite replays the boundary on a regtest
  transition chain (NU6.3 at `NU6_3_TRANSITION_BOUNDARY` = 6).

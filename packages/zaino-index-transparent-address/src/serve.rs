//! Address RPCs over one pinned [`ReadView`] per request
//!
//! - scan `receives`, then one batched `spent` lookup over every outpoint (unspent = a miss)
//! - synchronous: mmapped pages and segment walks, so a transport runs these off its async workers
//! - non-finalized rows reach the tip (a synced wallet's `tip - 1000` queries answer)

use std::num::NonZeroUsize;

use zaino_primitives::types::{Height, TransactionId, Zatoshis};
use zaino_sync::Served;
use zcash_protocol::consensus::NetworkType;
use zcash_transparent::address::TransparentAddress;

use crate::{key::AddressKey, view::ReadView};

/// - never an empty result (gap-limit walk must tell "no transactions" from "cannot tell")
/// - transport maps these onto gRPC codes; this crate names no transport
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// → `Unavailable` (clears on its own, so a client backs off); progress-free on purpose
    #[error("the transparent-address index is still syncing")]
    Syncing,

    /// Corrupt segments, not a bad request
    #[error("unspent total for this address exceeds the money supply")]
    SupplyExceeded,

    /// The request's addresses hold more receives than one request may walk (an exchange's,
    /// not a light wallet's); refused whole, never truncated (a short answer reads as a balance)
    #[error("these addresses have more than {limit} receives, the per-request limit")]
    TooManyRows { limit: usize },
}

/// Receives one request may walk unless an operator overrides it
///
/// - a light wallet's addresses hold tens to thousands; ~10 ms of segment walk at the limit
pub const DEFAULT_MAX_ADDRESS_ROWS: NonZeroUsize =
    NonZeroUsize::new(100_000).expect("100000 is non-zero");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressUtxo {
    pub height: u32,
    pub txid: TransactionId,
    pub vout: u32,
    pub value: Zatoshis,
}

/// Transaction paying the queried address or spending from it
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransactionRef {
    pub height: u32,
    pub txid: TransactionId,
}

#[derive(Debug, Clone)]
pub struct TransparentAddressService {
    served: Served<ReadView>,
    network: NetworkType,
    max_rows: NonZeroUsize,
}

impl TransparentAddressService {
    /// - unsynced → every method [`ServeError::Syncing`]
    /// - `network` = what the index was built for (its addresses are the only ones it answers)
    pub fn new(served: Served<ReadView>, network: NetworkType) -> Self {
        Self { served, network, max_rows: DEFAULT_MAX_ADDRESS_ROWS }
    }

    /// Overrides [`DEFAULT_MAX_ADDRESS_ROWS`]: receives one request may walk, across all its
    /// addresses (whole history, every method)
    pub fn with_max_rows(mut self, max_rows: NonZeroUsize) -> Self {
        self.max_rows = max_rows;
        self
    }

    fn too_many(&self) -> ServeError {
        ServeError::TooManyRows { limit: self.max_rows.get() }
    }

    pub fn network(&self) -> NetworkType {
        self.network
    }

    /// `GetAddressUtxos`: unspent receives from height `start` (inclusive) to the tip, oldest first
    pub fn utxos(
        &self,
        address: &TransparentAddress,
        start: Height,
    ) -> Result<Vec<AddressUtxo>, ServeError> {
        let mut utxos = self.utxos_of(std::slice::from_ref(address), start)?;
        Ok(utxos.pop().expect("one list per address"))
    }

    /// [`utxos`](Self::utxos) of each of `addresses`, in `addresses` order (one view, one
    /// batched spend lookup across all of them)
    pub fn utxos_of(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
    ) -> Result<Vec<Vec<AddressUtxo>>, ServeError> {
        let keys: Vec<AddressKey> = addresses.iter().map(AddressKey::from).collect();
        let unspent = self
            .pin()?
            .unspent(&keys, u32::from(start), self.max_rows.get())
            .ok_or_else(|| self.too_many())?;
        Ok(unspent
            .into_iter()
            .map(|rows| {
                rows.into_iter()
                    .map(|row| AddressUtxo {
                        height: row.key.height,
                        txid: row.key.txid,
                        vout: row.key.vout,
                        value: row.value,
                    })
                    .collect()
            })
            .collect())
    }

    /// `GetTaddressBalance`: sum of everything unspent, all of history
    pub fn balance(&self, address: &TransparentAddress) -> Result<Zatoshis, ServeError> {
        self.balance_of(address.into())
    }

    /// [`balance`](Self::balance) of each of `addresses`, in `addresses` order (one view, one
    /// batched spend lookup across all of them)
    pub fn balances(&self, addresses: &[TransparentAddress]) -> Result<Vec<Zatoshis>, ServeError> {
        let keys: Vec<AddressKey> = addresses.iter().map(AddressKey::from).collect();
        self.balances_of(&keys)
    }

    /// [`balance`](Self::balance) by storage key (reaches the opaque key no address parses to)
    pub(crate) fn balance_of(&self, key: AddressKey) -> Result<Zatoshis, ServeError> {
        Ok(self.balances_of(&[key])?.pop().expect("one balance per key"))
    }

    fn balances_of(&self, keys: &[AddressKey]) -> Result<Vec<Zatoshis>, ServeError> {
        self.pin()?
            .unspent(keys, 0, self.max_rows.get())
            .ok_or_else(|| self.too_many())?
            .into_iter()
            .map(|rows| {
                Zatoshis::sum_balances(rows.iter().map(|row| row.value))
                    .ok_or(ServeError::SupplyExceeded)
            })
            .collect()
    }

    /// `GetTaddressTransactions`: txs from height `start` to `end`, both inclusive, paying the
    /// address or spending from it
    ///
    /// - scanned over all of history, not the range (an in-range spend consumes an output received
    ///   at any earlier height; the outpoint = all the block carried)
    pub fn transactions(
        &self,
        address: &TransparentAddress,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionRef>, ServeError> {
        assert!(start <= end, "range {start}..={end} reversed (ordered at the router)");
        let pinned = self.pin()?;
        let (start, end) = (u32::from(start), u32::from(end));

        let received = pinned
            .receives(address.into(), 0, self.max_rows.get())
            .ok_or_else(|| self.too_many())?;
        let keys: Vec<_> = received.iter().map(|row| row.key).collect();
        let mut found = Vec::new();
        for (row, spend) in received.iter().zip(pinned.spends_of(&keys)) {
            if (start..=end).contains(&row.key.height) {
                found.push(TransactionRef { height: row.key.height, txid: row.key.txid });
            }

            let Some(spend) = spend else {
                continue;
            };
            if (start..=end).contains(&spend.height) {
                found.push(TransactionRef { height: spend.height, txid: spend.spender });
            }
        }

        // one tx can pay an address and spend from it, either one twice
        found.sort_unstable();
        found.dedup();

        Ok(found)
    }

    /// One consistent view for the request (no commit lands mid-answer)
    fn pin(&self) -> Result<std::sync::Arc<ReadView>, ServeError> {
        self.served.pin().ok_or(ServeError::Syncing)
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, sync::Arc, time::Duration};

    use super::*;
    use crate::TransparentAddressIndexWriter;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use zaino_chainview::{EndpointSet, QuorumTip};
    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        Block, BlockHash, BlockHeader, BlockRef, ReorgDepth, Script, Transaction, TransparentData,
        TransparentOutput,
    };
    use zaino_sync::{BlockSink, Step};

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// Syncing = a refusal on every method until the gate opens at chainview's tip; an address
    /// with no history = a successful empty answer, never an error
    #[tokio::test]
    async fn syncing_refuses_and_an_empty_history_answers_empty() {
        let address = TransparentAddress::PublicKeyHash([0x01; 20]);
        let within = Duration::from_secs(10);

        let index = TransparentAddressIndexWriter::open(
            SimFs::new(),
            std::path::Path::new("/ta"),
            zcash_protocol::consensus::NetworkType::Regtest,
            NonZeroUsize::new(1 << 20).expect("non-zero"),
        )
        .expect("open");
        let service = TransparentAddressService::new(
            index.published().served(),
            zcash_protocol::consensus::NetworkType::Regtest,
        );
        let (mut synced, mut durable) =
            (index.published().subscribe_synced(), index.published().subscribe_finalized());
        // chainview's quorum tip at height 1: the gate opens once the index applies it
        let block = BlockRef { hash: BlockHash::from([1; 32]), height: h(1) };
        let (_tips, tips) =
            watch::channel(Some(QuorumTip { block, agreed_by: EndpointSet::default() }));
        let depth = ReorgDepth::new(NonZeroU32::new(10).expect("non-zero"));
        let cancel = CancellationToken::new();
        let gate = tokio::spawn(index.published().gate(tips, depth, cancel.clone()));

        // unsynced: every method refuses alike, none naming a height or a tip
        assert_eq!(service.balance(&address), Err(ServeError::Syncing));
        assert_eq!(service.utxos(&address, h(0)), Err(ServeError::Syncing));
        assert_eq!(service.transactions(&address, h(0), h(10)), Err(ServeError::Syncing));

        let mut sink = BlockSink::new("blocks");
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, queue);
        let running = tokio::spawn(index.run(blocks, CancellationToken::new()));
        // heights 0 and 1, only 1 pays the address (0 = bare coinbase)
        for height in 0..2u32 {
            let (txid, outputs) = if height == 1 {
                let paid = TransparentOutput {
                    value: Zatoshis::new(42).expect("in supply"),
                    script: Script::new(
                        [&[0x76, 0xa9, 0x14][..], &[0x01; 20], &[0x88, 0xac]].concat(),
                    ),
                };
                (0x77, vec![paid])
            } else {
                (0xc0, Vec::new())
            };
            let transactions = vec![Transaction {
                txid: TransactionId::from([txid; 32]),
                transparent: TransparentData { coinbase: false, inputs: Vec::new(), outputs },
                sprout: Default::default(),
                sapling: Default::default(),
                orchard: Default::default(),
                ironwood: Default::default(),
            }];

            let block = Arc::new(Block::new(
                BlockHeader::for_tests(
                    height,
                    [height as u8; 32],
                    [height.wrapping_sub(1) as u8; 32],
                    1_700_000_000 + height,
                ),
                transactions,
            ));
            let height = block.header().height;
            sink.send(Step::Apply { height, finalized: false, data: block }).await;
        }
        let open = tokio::time::timeout(within, synced.wait_for(|open| *open)).await;
        open.expect("gate opens at the tip").expect("gate alive");

        // applied, uncommitted: non-finalized served (same answer either side of a commit)
        let paid = Zatoshis::new(42).expect("in supply");
        assert_eq!(
            service.balance(&address),
            Ok(paid),
            "non-finalized rows = answers, not a batch"
        );

        for height in [h(0), h(1)] {
            sink.send(Step::Finalized { height }).await;
        }
        let one = tokio::time::timeout(within, durable.wait_for(|at| *at == Some(h(1)))).await;
        one.expect("1 written").expect("index alive");

        // applied to 2: heights past the fold absent, not an error (a synced index answers)
        assert_eq!(service.transactions(&address, h(2), h(2)), Ok(Vec::new()));
        assert_eq!(service.utxos(&address, h(2)), Ok(Vec::new()));

        // inside the built range, an address nobody paid = a successful empty answer
        let stranger = TransparentAddress::ScriptHash([0xff; 20]);
        assert_eq!(service.utxos(&stranger, h(0)), Ok(Vec::new()));
        assert_eq!(service.transactions(&stranger, h(0), h(1)), Ok(Vec::new()));
        assert_eq!(service.balance(&stranger), Ok(Zatoshis::ZERO));

        // paid address: its one row
        assert_eq!(service.balance(&address), Ok(paid));
        let paying = TransactionRef { height: 1, txid: TransactionId::from([0x77; 32]) };
        assert_eq!(service.transactions(&address, h(0), h(1)), Ok(vec![paying]));

        sink.shutdown();
        running.await.expect("no panic").expect("followed through Shutdown");
        cancel.cancel();
        gate.await.expect("gate ends on cancel");
    }

    /// The row budget counts receives across every address of a request, on both tiers: at the
    /// limit every method answers, one over it every method refuses whole (never a short list or
    /// a partial balance)
    #[tokio::test]
    async fn a_request_over_its_row_budget_is_refused_whole_by_every_method() {
        let (first, second) = (
            TransparentAddress::PublicKeyHash([0x01; 20]),
            TransparentAddress::PublicKeyHash([0x02; 20]),
        );
        let pays = |hash: u8| TransparentOutput {
            value: Zatoshis::new(10).expect("in supply"),
            script: Script::new([&[0x76, 0xa9, 0x14][..], &[hash; 20], &[0x88, 0xac]].concat()),
        };

        let index = TransparentAddressIndexWriter::open(
            SimFs::new(),
            std::path::Path::new("/ta"),
            zcash_protocol::consensus::NetworkType::Regtest,
            NonZeroUsize::new(1 << 20).expect("non-zero"),
        )
        .expect("open");
        let published = index.published().served();
        let (mut applied, mut durable) =
            (index.published().subscribe_applied(), index.published().subscribe_finalized());
        let mut sink = BlockSink::new("blocks");
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        let subscription = sink.subscribe(TransparentAddressIndexWriter::NAME, queue);
        let running = tokio::spawn(index.run(subscription, CancellationToken::new()));
        // heights 0 to 2 (both inclusive) sent final (bulk), 3 non-final (written at the bulk →
        // tip handoff, so 0..=2 durable): `first` paid at 1, 2 and 3, `second` at 2
        let mut blocks = Vec::new();
        for height in 0..4u32 {
            let outputs = match height {
                0 => Vec::new(),
                2 => vec![pays(0x01), pays(0x02)],
                _ => vec![pays(0x01)],
            };
            blocks.push(Arc::new(Block::new(
                BlockHeader::for_tests(
                    height,
                    [height as u8; 32],
                    [height.wrapping_sub(1) as u8; 32],
                    1_700_000_000 + height,
                ),
                vec![Transaction {
                    txid: TransactionId::from([0x70 + height as u8; 32]),
                    transparent: TransparentData { coinbase: false, inputs: Vec::new(), outputs },
                    sprout: Default::default(),
                    sapling: Default::default(),
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }],
            )));
        }
        for (height, block) in (0u32..).zip(&blocks) {
            let (height, finalized) = (h(height), height < 3);
            sink.send(Step::Apply { height, finalized, data: Arc::clone(block) }).await;
        }
        let within = Duration::from_secs(10);
        let three = tokio::time::timeout(within, applied.wait_for(|at| *at == Some(h(3)))).await;
        three.expect("3 applied").expect("index alive");
        assert_eq!(*durable.borrow_and_update(), Some(h(2)), "bulk written before 3 applies on it");

        let served = Served::fixed((*published.pin_any()).clone());
        let network = zcash_protocol::consensus::NetworkType::Regtest;
        let budget = |rows: usize| {
            TransparentAddressService::new(served.clone(), network)
                .with_max_rows(NonZeroUsize::new(rows).expect("non-zero"))
        };
        let over = |rows: usize| Some(ServeError::TooManyRows { limit: rows });
        let both = [first, second];

        // `first` alone = 3 rows (2 durable + 1 non-finalized); both = 4
        let at = budget(3);
        assert_eq!(at.balance(&first), Ok(Zatoshis::new(30).expect("in supply")));
        assert_eq!(at.utxos(&first, h(0)).map(|rows| rows.len()), Ok(3));
        assert_eq!(at.transactions(&first, h(0), h(3)).map(|found| found.len()), Ok(3));
        assert_eq!(at.balances(&both).err(), over(3), "3 + 1 across the request's addresses");

        let under = budget(2);
        assert_eq!(under.balance(&first).err(), over(2));
        assert_eq!(under.utxos(&first, h(0)).err(), over(2));
        let narrow = under.transactions(&first, h(3), h(3)).err();
        assert_eq!(narrow, over(2), "a narrow range still walks the whole history");
        assert_eq!(under.utxos(&first, h(2)).map(|rows| rows.len()), Ok(2), "from 2: 2 rows");

        let roomy = budget(4);
        let sums = roomy.balances(&both).expect("4 rows fit");
        assert_eq!(sums, [30, 10].map(|zat| Zatoshis::new(zat).expect("in supply")));

        sink.shutdown();
        running.await.expect("no panic").expect("followed through Shutdown");
    }
}

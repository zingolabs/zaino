//! `Arc` forwarding for the ports the compact indexer consumes.
//!
//! One shared `Arc<V>` backs both driven-port consumers in the composed runtime:
//! the chain-head reaches the raw [`OneShotGetChainTip`] / block ports through
//! the `Arc`'s `Deref`, while the FS indexer wraps the *same* `Arc<V>` in a
//! [`ValidatorClient`](crate::ValidatorClient), whose resilient port impls require
//! the wrapped type — the `Arc` itself — to provide the matching `OneShot*`
//! ports. These mechanical `Deref`-forwards let a bare `Arc<V>` satisfy those
//! bounds, so a single validator handle serves both consumers without cloning
//! the adapter.
//!
//! The forwarded set is exactly the ports the shared `Arc<V>` must satisfy for
//! the two consumers: [`ValidatorSource`] (the shared supertrait) plus the
//! compact-indexer path ([`OneShotGetPreIndexCompactBlock`],
//! [`OneShotGetChainTip`], [`SubscribeChainTip`]) and the serving path's
//! passthrough ports ([`OneShotGetTreestate`], [`OneShotSendRawTransaction`],
//! and the transparent-address reads). Each is a mechanical `Deref`-forward.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;

use zaino_primitives::types::{
    AddressBalance, AddressDelta, BlockHash, Height, PreIndexCompactBlock, TransactionId,
    Treestate, Utxo,
};

use crate::{
    GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError, GetAddressUtxosError,
    GetBlockError, GetChainTipError, GetTreestateError, OneShotGetAddressBalance,
    OneShotGetAddressDeltas, OneShotGetAddressTxids, OneShotGetAddressUtxos, OneShotGetChainTip,
    OneShotGetPreIndexCompactBlock, OneShotGetTreestate, OneShotSendRawTransaction, QueryError,
    SendRawTransactionError, SubscribeChainTip, TipObservation, ValidatorSource,
};

impl<V: ValidatorSource + ?Sized> ValidatorSource for Arc<V> {
    type NonDomain = V::NonDomain;
}

impl<V: OneShotGetPreIndexCompactBlock + ?Sized> OneShotGetPreIndexCompactBlock for Arc<V> {
    fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<PreIndexCompactBlock, QueryError<GetBlockError, Self::NonDomain>>>
           + Send {
        (**self).get_pre_index_compact_block(height)
    }
}

impl<V: OneShotGetChainTip + ?Sized> OneShotGetChainTip for Arc<V> {
    fn get_chain_tip(
        &self,
    ) -> impl Future<
        Output = Result<(BlockHash, Height), QueryError<GetChainTipError, Self::NonDomain>>,
    > + Send {
        (**self).get_chain_tip()
    }
}

impl<V: SubscribeChainTip + ?Sized> SubscribeChainTip for Arc<V> {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        (**self).subscribe_to_chain_tip()
    }
}

// The serving path's passthrough reads reach the validator through the same
// `Arc<V>`: treestate today, more as the read-set is wired.
impl<V: OneShotGetTreestate + ?Sized> OneShotGetTreestate for Arc<V> {
    fn get_treestate(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Treestate, QueryError<GetTreestateError, Self::NonDomain>>> + Send
    {
        (**self).get_treestate(height)
    }
}

// The serving path's broadcast control reaches the validator's send port through
// the same `Arc<V>`, so the composed engine can relay a wallet's transaction
// without holding the concrete adapter.
impl<V: OneShotSendRawTransaction + ?Sized> OneShotSendRawTransaction for Arc<V> {
    fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<
        Output = Result<TransactionId, QueryError<SendRawTransactionError, Self::NonDomain>>,
    > + Send {
        (**self).send_raw_transaction(transaction)
    }
}

// The serving path's transparent-address reads reach the validator through the
// same `Arc<V>` (a passthrough stopgap pending a local transparent index).
impl<V: OneShotGetAddressBalance + ?Sized> OneShotGetAddressBalance for Arc<V> {
    fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> impl Future<
        Output = Result<AddressBalance, QueryError<GetAddressBalanceError, Self::NonDomain>>,
    > + Send {
        (**self).get_address_balance(addresses)
    }
}

impl<V: OneShotGetAddressUtxos + ?Sized> OneShotGetAddressUtxos for Arc<V> {
    fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> impl Future<Output = Result<Vec<Utxo>, QueryError<GetAddressUtxosError, Self::NonDomain>>> + Send
    {
        (**self).get_address_utxos(addresses)
    }
}

impl<V: OneShotGetAddressTxids + ?Sized> OneShotGetAddressTxids for Arc<V> {
    fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> impl Future<
        Output = Result<Vec<TransactionId>, QueryError<GetAddressTxidsError, Self::NonDomain>>,
    > + Send {
        (**self).get_address_txids(addresses, start, end)
    }
}

impl<V: OneShotGetAddressDeltas + ?Sized> OneShotGetAddressDeltas for Arc<V> {
    fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> impl Future<
        Output = Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError, Self::NonDomain>>,
    > + Send {
        (**self).get_address_deltas(addresses, start, end)
    }
}

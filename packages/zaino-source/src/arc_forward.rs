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
//! Only the ports the compact-indexer path needs are forwarded:
//! [`ValidatorSource`] (the shared supertrait), [`OneShotGetPreIndexCompactBlock`],
//! [`OneShotGetChainTip`], and [`SubscribeChainTip`] — the exact set
//! `ValidatorClient`'s resilient `GetPreIndexCompactBlock` / `GetChainTip`
//! delegate to, plus the tip subscription the driver forwards unchanged.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;

use zaino_primitives::types::{BlockHash, Height, PreIndexCompactBlock, TransactionId};

use crate::{
    GetBlockError, GetChainTipError, OneShotGetChainTip, OneShotGetPreIndexCompactBlock,
    OneShotSendRawTransaction, QueryError, SendRawTransactionError, SubscribeChainTip,
    TipObservation, ValidatorSource,
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

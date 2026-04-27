//! validator connected blockchain source.

use super::*;

macro_rules! expected_read_response {
    ($response:ident, $expected_variant:ident) => {
        match $response {
            ReadResponse::$expected_variant(inner) => inner,
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        }
    };
}

/// ReadStateService based validator connector.
///
/// Currently the Mempool cannot utilise the mempool change endpoint in the ReadStateService,
/// for this reason the lagacy jsonrpc inteface is used until the Mempool updates required can be implemented.
///
/// Due to the difference if the mempool inteface provided by the ReadStateService and the Json RPC service
/// two seperate Mempool implementation will likely be required.
#[derive(Clone, Debug)]
pub struct State {
    /// Used to fetch chain data.
    pub read_state_service: ReadStateService,
    /// Temporarily used to fetch mempool data.
    pub mempool_fetcher: JsonRpSeeConnector,
    /// Current network type being run.
    pub network: Network,
}

/// A connection to a validator.
#[derive(Clone, Debug)]
// TODO: Explore whether State should be Boxed.
#[allow(clippy::large_enum_variant)]
pub enum ValidatorConnector {
    /// The connection is via direct read access to a zebrad's data file
    ///
    /// NOTE: See docs for State struct.
    State(State),
    /// We are connected to a zebrad, zcashd, or other zainod via JsonRpc ("JsonRpSee")
    Fetch(JsonRpSeeConnector),
}

#[async_trait]
impl BlockchainSource for ValidatorConnector {
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match self {
            ValidatorConnector::State(state) => match state
                .read_state_service
                .clone()
                .call(zebra_state::ReadRequest::Block(id))
                .await
            {
                Ok(zebra_state::ReadResponse::Block(Some(block))) => Ok(Some(block)),
                // Zebra's ReadStateService does not currently serve non-best chain blocks
                // so we must fetch using the JsonRpcConnector.
                Ok(zebra_state::ReadResponse::Block(None)) => {
                    match state.mempool_fetcher.get_block(id.to_string(), Some(0)).await
                    {
                        Ok(GetBlockResponse::Raw(raw_block)) => Ok(Some(Arc::new(
                            zebra_chain::block::Block::zcash_deserialize(raw_block.as_ref())
                                .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                            ))),
                        Ok(_) => unreachable!(),
                        Err(e) => match e {
                            RpcRequestError::Method(GetBlockError::MissingBlock(_)) => Ok(None),
                            // TODO/FIX: zcashd returns this transport error when a block is requested higher than current chain. is this correct?
                            RpcRequestError::Transport(zaino_fetch::jsonrpsee::error::TransportError::ErrorStatusCode(500)) => Ok(None),
                            RpcRequestError::ServerWorkQueueFull => Err(BlockchainSourceError::Unrecoverable("Work queue full. not yet implemented: handling of ephemeral network errors.".to_string())),
                            _ => Err(BlockchainSourceError::Unrecoverable(e.to_string())),
                        },
                    }
                }
                Ok(otherwise) => panic!(
                    "Read Request of Block returned Read Response of {otherwise:#?} \n\
                    This should be deterministically unreachable"
                ),
                Err(e) => Err(BlockchainSourceError::Unrecoverable(e.to_string())),
            },
            ValidatorConnector::Fetch(fetch) => {
                match fetch
                    .get_block(id.to_string(), Some(0))
                    .await
                {
                    Ok(GetBlockResponse::Raw(raw_block)) => Ok(Some(Arc::new(
                        zebra_chain::block::Block::zcash_deserialize(raw_block.as_ref())
                            .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                    ))),
                    Ok(_) => unreachable!(),
                    Err(e) => match e {
                        RpcRequestError::Method(GetBlockError::MissingBlock(_)) => Ok(None),
                        // TODO/FIX: zcashd returns this transport error when a block is requested higher than current chain. is this correct?
                        RpcRequestError::Transport(zaino_fetch::jsonrpsee::error::TransportError::ErrorStatusCode(500)) => Ok(None),
                        RpcRequestError::ServerWorkQueueFull => Err(BlockchainSourceError::Unrecoverable("Work queue full. not yet implemented: handling of ephemeral network errors.".to_string())),
                        _ => Err(BlockchainSourceError::Unrecoverable(e.to_string())),
                    },
                }
            }
        }
    }

    async fn get_commitment_tree_roots(
        &self,
        // Sould this be HashOrHeight?
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        match self {
            ValidatorConnector::State(state) => {
                let (sapling_tree_response, orchard_tree_response) =
                    join(
                        state.read_state_service.clone().call(
                            zebra_state::ReadRequest::SaplingTree(HashOrHeight::Hash(id.into())),
                        ),
                        state.read_state_service.clone().call(
                            zebra_state::ReadRequest::OrchardTree(HashOrHeight::Hash(id.into())),
                        ),
                    )
                    .await;
                let (sapling_tree, orchard_tree) = match (
                    //TODO: Better readstateservice error handling
                    sapling_tree_response
                        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                    orchard_tree_response
                        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                ) {
                    (ReadResponse::SaplingTree(saptree), ReadResponse::OrchardTree(orctree)) => {
                        (saptree, orctree)
                    }
                    (_, _) => panic!("Bad response"),
                };

                Ok((
                    sapling_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                    orchard_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                ))
            }
            ValidatorConnector::Fetch(fetch) => {
                let tree_responses = fetch
                    .get_treestate(id.to_rpc_hex())
                    .await
                    // As MethodError contains a GetTreestateError, which is an enum with no variants,
                    // we don't need to account for it at all here
                    .map_err(|e| match e {
                        RpcRequestError::ServerWorkQueueFull => {
                            BlockchainSourceError::Unrecoverable(
                                "Not yet implemented: handle backing validator\
                                full queue"
                                    .to_string(),
                            )
                        }
                        _ => BlockchainSourceError::Unrecoverable(e.to_string()),
                    })?;
                let GetTreestateResponse {
                    sapling, orchard, ..
                } = tree_responses;
                let sapling_frontier = sapling
                    .map_or_else(
                        || Some(Ok(CommitmentTree::empty())),
                        |t| {
                            t.commitments().final_state().as_ref().map(|final_state| {
                                read_commitment_tree::<sapling_crypto::Node, _, 32>(
                                    final_state.as_slice(),
                                )
                            })
                        },
                    )
                    .transpose()
                    .map_err(|e| BlockchainSourceError::Unrecoverable(format!("io error: {e}")))?;
                let orchard_frontier = orchard
                    .map_or_else(
                        || Some(Ok(CommitmentTree::empty())),
                        |t| {
                            t.commitments().final_state().as_ref().map(|final_state| {
                                read_commitment_tree::<zebra_chain::orchard::tree::Node, _, 32>(
                                    final_state.as_slice(),
                                )
                            })
                        },
                    )
                    .transpose()
                    .map_err(|e| BlockchainSourceError::Unrecoverable(format!("io error: {e}")))?;
                let sapling_root = sapling_frontier
                    .map(|tree| {
                        zebra_chain::sapling::tree::Root::try_from(tree.root().to_bytes())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("could not deser: {e}"))
                    })?;
                let orchard_root = orchard_frontier
                    .map(|tree| {
                        zebra_chain::orchard::tree::Root::try_from(tree.root().to_repr())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("could not deser: {e}"))
                    })?;
                Ok((sapling_root, orchard_root))
            }
        }
    }

    /// Returns the Sapling and Orchard treestate by blockhash.
    async fn get_treestate(
        &self,
        // Sould this be HashOrHeight?
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        let hash_or_height: HashOrHeight = HashOrHeight::Hash(zebra_chain::block::Hash(id.into()));
        match self {
            ValidatorConnector::State(state) => {
                let mut state = state.clone();
                let block_header_response = state
                    .read_state_service
                    .ready()
                    .and_then(|service| service.call(ReadRequest::BlockHeader(hash_or_height)))
                    .await
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not fetch header of block {id}"))
                                .to_string(),
                        )
                    })?;
                let (_header, _hash, height) = match block_header_response {
                    ReadResponse::BlockHeader {
                        header,
                        hash,
                        height,
                        ..
                    } => (header, hash, height),
                    unexpected => {
                        unreachable!("Unexpected response from state service: {unexpected:?}")
                    }
                };

                let sapling = match zebra_chain::parameters::NetworkUpgrade::Sapling
                    .activation_height(&state.network.to_zebra_network())
                {
                    Some(activation_height) if height >= activation_height => Some(
                        state
                            .read_state_service
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::SaplingTree(hash_or_height))
                            })
                            .await
                            .map_err(|_e| {
                                BlockchainSourceError::Unrecoverable(
                                    InvalidData(format!(
                                        "could not fetch sapling treestate of block {id}"
                                    ))
                                    .to_string(),
                                )
                            })?,
                    ),
                    _ => None,
                }
                .and_then(|sap_response| {
                    expected_read_response!(sap_response, SaplingTree)
                        .map(|tree| tree.to_rpc_bytes())
                });

                let orchard = match zebra_chain::parameters::NetworkUpgrade::Nu5
                    .activation_height(&state.network.to_zebra_network())
                {
                    Some(activation_height) if height >= activation_height => Some(
                        state
                            .read_state_service
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::OrchardTree(hash_or_height))
                            })
                            .await
                            .map_err(|_e| {
                                BlockchainSourceError::Unrecoverable(
                                    InvalidData(format!(
                                        "could not fetch orchard treestate of block {id}"
                                    ))
                                    .to_string(),
                                )
                            })?,
                    ),
                    _ => None,
                }
                .and_then(|orch_response| {
                    expected_read_response!(orch_response, OrchardTree)
                        .map(|tree| tree.to_rpc_bytes())
                });

                Ok((sapling, orchard))
            }
            ValidatorConnector::Fetch(fetch) => {
                let treestate = fetch
                    .get_treestate(hash_or_height.to_string())
                    .await
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not fetch treestate of block {id}"))
                                .to_string(),
                        )
                    })?;

                let sapling = treestate.sapling.map_or_else(
                    || {
                        let mut tree = vec![];
                        write_commitment_tree(&sapling_crypto::CommitmentTree::empty(), &mut tree)
                            .expect("can write to Vec");
                        Some(tree)
                    },
                    |t| t.commitments().final_state().clone(),
                );

                let orchard = treestate.orchard.map_or_else(
                    || {
                        let mut tree = vec![];
                        write_commitment_tree(
                            &CommitmentTree::<zebra_chain::orchard::tree::Node, 32>::empty(),
                            &mut tree,
                        )
                        .expect("can write to Vec");
                        Some(tree)
                    },
                    |t| t.commitments().final_state().clone(),
                );

                Ok((sapling, orchard))
            }
        }
    }

    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        let mempool_fetcher = match self {
            ValidatorConnector::State(state) => &state.mempool_fetcher,
            ValidatorConnector::Fetch(fetch) => fetch,
        };

        let txid_strings = mempool_fetcher
            .get_raw_mempool()
            .await
            .map_err(|e| {
                BlockchainSourceError::Unrecoverable(format!("could not fetch mempool data: {e}"))
            })?
            .transactions;

        let txids: Vec<zebra_chain::transaction::Hash> = txid_strings
            .into_iter()
            .map(|txid_str| {
                zebra_chain::transaction::Hash::from_str(&txid_str).map_err(|e| {
                    BlockchainSourceError::Unrecoverable(format!(
                        "invalid transaction id '{txid_str}': {e}"
                    ))
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(Some(txids))
    }

    // Returns the transaction, and the height of the block that transaction is in if on the best chain
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    > {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                network: _,
            }) => {
                // Check state for transaction
                let mut read_state_service = read_state_service.clone();
                let mempool_fetcher = mempool_fetcher.clone();

                let zebra_txid: zebra_chain::transaction::Hash =
                    zebra_chain::transaction::Hash::from(txid.0);

                let response = read_state_service
                    .ready()
                    .and_then(|svc| {
                        svc.call(zebra_state::ReadRequest::AnyChainTransaction(zebra_txid))
                    })
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("state read failed: {e}"))
                    })?;

                if let zebra_state::ReadResponse::AnyChainTransaction(opt) = response {
                    if let Some(any_chain_tx) = opt {
                        match any_chain_tx {
                            zebra_state::AnyTx::Mined(mined_tx) => {
                                return Ok(Some((
                                    (mined_tx).tx.clone(),
                                    GetTransactionLocation::BestChain(mined_tx.height),
                                )))
                            }
                            zebra_state::AnyTx::Side((transaction, _block_hash)) => {
                                return Ok(Some((
                                    transaction,
                                    GetTransactionLocation::NonbestChain,
                                )))
                            }
                        }
                    }
                } else {
                    unreachable!("unmatched response to a `Transaction` read request");
                }

                // Else check mempool for transaction.
                let mempool_txids = self.get_mempool_txids().await?.ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable(
                        "could not fetch mempool transaction ids: none returned".to_string(),
                    )
                })?;
                if mempool_txids.contains(&zebra_txid) {
                    let serialized_transaction = if let GetTransactionResponse::Raw(
                        serialized_transaction,
                    ) = mempool_fetcher
                        .get_raw_transaction(zebra_txid.to_string(), Some(0))
                        .await
                        .map_err(|e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "could not fetch transaction data: {e}"
                            ))
                        })? {
                        serialized_transaction
                    } else {
                        return Err(BlockchainSourceError::Unrecoverable(
                            "could not fetch transaction data: non-raw response".to_string(),
                        ));
                    };
                    let transaction: zebra_chain::transaction::Transaction =
                        zebra_chain::transaction::Transaction::zcash_deserialize(
                            std::io::Cursor::new(serialized_transaction.as_ref()),
                        )
                        .map_err(|e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "could not deserialize transaction data: {e}"
                            ))
                        })?;
                    Ok(Some((transaction.into(), GetTransactionLocation::Mempool)))
                } else {
                    Ok(None)
                }
            }
            ValidatorConnector::Fetch(fetch) => {
                let transaction_object = if let GetTransactionResponse::Object(transaction_object) =
                    fetch
                        .get_raw_transaction(txid.to_rpc_hex(), Some(1))
                        .await
                        .map_err(|e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "could not fetch transaction data: {e}"
                            ))
                        })? {
                    transaction_object
                } else {
                    return Err(BlockchainSourceError::Unrecoverable(
                        "could not fetch transaction data: non-obj response".to_string(),
                    ));
                };
                let transaction: zebra_chain::transaction::Transaction =
                    zebra_chain::transaction::Transaction::zcash_deserialize(std::io::Cursor::new(
                        transaction_object.hex().as_ref(),
                    ))
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not deserialize transaction data: {e}"
                        ))
                    })?;
                let location = match transaction_object.height() {
                    Some(-1) => GetTransactionLocation::NonbestChain,
                    None => GetTransactionLocation::Mempool,
                    Some(n) => {
                        GetTransactionLocation::BestChain(n.try_into_height().map_err(|_e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "invalid height value {n}"
                            ))
                        })?)
                    }
                };
                Ok(Some((transaction.into(), location)))
            }
        }
    }

    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                network: _,
            }) => {
                match read_state_service.best_tip() {
                    Some((_height, hash)) => Ok(Some(hash)),
                    None => {
                        // try RPC if state read fails:
                        Ok(Some(
                            mempool_fetcher
                                .get_best_blockhash()
                                .await
                                .map_err(|e| {
                                    BlockchainSourceError::Unrecoverable(format!(
                                        "could not fetch best block hash from validator: {e}"
                                    ))
                                })?
                                .0,
                        ))
                    }
                }
            }
            ValidatorConnector::Fetch(fetch) => Ok(Some(
                fetch
                    .get_best_blockhash()
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not fetch best block hash from validator: {e}"
                        ))
                    })?
                    .0,
            )),
        }
    }

    /// Returns the height of the block at the tip of the best chain.
    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                network: _,
            }) => {
                match read_state_service.best_tip() {
                    Some((height, _hash)) => Ok(Some(height)),
                    None => {
                        // try RPC if state read fails:
                        Ok(Some(
                            mempool_fetcher
                                .get_block_count()
                                .await
                                .map_err(|e| {
                                    BlockchainSourceError::Unrecoverable(format!(
                                        "could not fetch best block hash from validator: {e}"
                                    ))
                                })?
                                .into(),
                        ))
                    }
                }
            }
            ValidatorConnector::Fetch(fetch) => Ok(Some(
                fetch
                    .get_block_count()
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not fetch best block hash from validator: {e}"
                        ))
                    })?
                    .into(),
            )),
        }
    }

    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher: _,
                network: _,
            }) => {
                match read_state_service
                    .clone()
                    .call(zebra_state::ReadRequest::NonFinalizedBlocksListener)
                    .await
                {
                    Ok(ReadResponse::NonFinalizedBlocksListener(listener)) => {
                        // NOTE:  This is not Option::unwrap, but a custom zebra-defined NonFinalizedBlocksListener::unwrap.
                        Ok(Some(listener.unwrap()))
                    }
                    Ok(_) => unreachable!(),
                    Err(e) => Err(e),
                }
            }
            ValidatorConnector::Fetch(_fetch) => Ok(None),
        }
    }

    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        match self {
            ValidatorConnector::State(state) => {
                let start_index = NoteCommitmentSubtreeIndex(start_index);
                let limit = max_entries.map(NoteCommitmentSubtreeIndex);
                let request = match pool {
                    ShieldedPool::Sapling => ReadRequest::SaplingSubtrees { start_index, limit },
                    ShieldedPool::Orchard => ReadRequest::OrchardSubtrees { start_index, limit },
                };
                state
                    .read_state_service
                    .clone()
                    .call(request)
                    .await
                    .map(|response| match pool {
                        ShieldedPool::Sapling => expected_read_response!(response, SaplingSubtrees)
                            .iter()
                            .map(|(_index, subtree)| {
                                (subtree.root.to_bytes(), subtree.end_height.0)
                            })
                            .collect(),
                        ShieldedPool::Orchard => expected_read_response!(response, OrchardSubtrees)
                            .iter()
                            .map(|(_index, subtree)| (subtree.root.to_repr(), subtree.end_height.0))
                            .collect(),
                    })
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not get subtrees from validator: {e}"
                        ))
                    })
            }

            ValidatorConnector::Fetch(json_rp_see_connector) => {
                let subtrees = json_rp_see_connector
                    .get_subtrees_by_index(pool.pool_string(), start_index, max_entries)
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not get subtrees from validator: {e}"
                        ))
                    })?;

                Ok(subtrees
                    .subtrees
                    .iter()
                    .map(|subtree| {
                        Ok::<_, Box<dyn Error + Send + Sync>>((
                            <[u8; 32]>::try_from(hex::decode(&subtree.root)?).map_err(
                                |_subtree| {
                                    std::io::Error::new(
                                        std::io::ErrorKind::InvalidInput,
                                        "received subtree root not 32 bytes",
                                    )
                                },
                            )?,
                            subtree.end_height.0,
                        ))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not get subtrees from validator: {e}"
                        ))
                    })?)
            }
        }
    }
}

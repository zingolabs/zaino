//! Commitment treestate and subtree roots over the finalised store.
//!
//! The finalised tier answers a treestate read from the `tree_state` index — one
//! per-pool note-commitment frontier at every height — and a subtree-roots read
//! from the three per-pool `subtrees_*` namespaces. Both are
//! [`WalkOrdered`](zaino_persistence_codec::KeyOrder::WalkOrdered), so they are
//! never deferred during the initial catch-up: the store's coverage is exactly
//! its watermark, and a height above it is [`NotServiceable`], never an empty
//! tree. The non-finalised window above the watermark is a separate tier; the
//! composer in `zaino-core` folds the two across the seam.
//!
//! # As of the watermark
//!
//! A treestate is reported against its block, so the hash and time come from the
//! `headers` index at the same height; the frontier's projection onto the domain
//! value (the serialized tree, its root, and the per-pool activation proxy) is
//! shared with the window through [`pool_treestate`](zaino_indexes::indexes::tree_state::pool_treestate),
//! so the two sides of the seam render identically.
//!
//! [`NotServiceable`]: TreestateReadError::NotServiceable

use std::ops::ControlFlow;

use zaino_indexes::capabilities::local::{self, Backs};
use zaino_indexes::indexes::headers::{self, HeadersIndex};
use zaino_indexes::indexes::subtrees::{
    IronwoodSubtreesIndex, OrchardSubtreesIndex, SaplingSubtreesIndex,
};
use zaino_indexes::indexes::tree_state::{self, TreeStateIndex};
use zaino_persistence::{Backend, BackendReader, Namespace};
use zaino_persistence_codec::{decode_value, encode_key, EntryCodec};
use zaino_primitives::types::{
    Height, PoolActivations, ShieldedPool, SubtreeRoot, TreeRoot, Treestate,
};
use zaino_service::error::TreestateReadError;
use zaino_service::{Capability, PoolActivationSource, TreestateRead};
use zaino_sync::traits::IndexDef;

use crate::{read_index_value, serviceability_gate, StoreSnapshot};

impl<B, M> TreestateRead for StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::Treestate> + Backs<local::SubtreeRoots>,
{
    async fn treestate(&self, at: Height) -> Result<Treestate, TreestateReadError> {
        // `tree_state` is WalkOrdered, so the store holds it for every height up
        // to the pinned watermark and for none above it. A height above coverage
        // is "not built to here yet" — NotServiceable, never an empty tree.
        match self.watermark {
            Some(watermark) if at <= watermark => {}
            _ => return Err(TreestateReadError::NotServiceable(Capability::Treestate)),
        }

        let reader = self.treestate_reader()?;
        // Defensive readiness gate: WalkOrdered namespaces never defer, but the
        // completeness probe keeps this read honest if that ever changes, and
        // costs one check per read rather than per entry.
        treestate_gate::<B>(&reader)?;

        let value =
            read_index_value::<TreeStateIndex, B>(&reader, tree_state::codec::ID.into(), at)
                .map_err(|t| TreestateReadError::Transient(t.0))?
                .ok_or_else(|| {
                    // In coverage yet absent: the watermark claims this height is
                    // built, so a missing frontier is index corruption, not a
                    // height the store does not cover. Fail loud.
                    TreestateReadError::Fatal(format!(
                        "tree_state missing at height {} (at or below the watermark)",
                        u32::from(at),
                    ))
                })?;
        let header = read_index_value::<HeadersIndex, B>(&reader, headers::ID.into(), at)
            .map_err(|t| TreestateReadError::Transient(t.0))?
            .ok_or_else(|| {
                TreestateReadError::Fatal(format!(
                    "headers missing at height {} (at or below the watermark)",
                    u32::from(at),
                ))
            })?;

        let (sapling, orchard, ironwood) = value.pool_treestates(&self.activations, at);
        Ok(Treestate {
            block_hash: header.hash,
            height: at,
            time: header.time,
            sapling,
            orchard,
            ironwood,
        })
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        let reader = self.treestate_reader()?;
        subtree_roots_gate::<B>(&reader)?;
        match pool {
            ShieldedPool::Sapling => {
                scan_subtrees::<SaplingSubtreesIndex, B>(&reader, start_index, limit)
            }
            ShieldedPool::Orchard => {
                scan_subtrees::<OrchardSubtreesIndex, B>(&reader, start_index, limit)
            }
            ShieldedPool::Ironwood => {
                scan_subtrees::<IronwoodSubtreesIndex, B>(&reader, start_index, limit)
            }
        }
    }
}

/// The store reports the schedule it was built with, so the composer can seed the
/// window's fold with the same activation boundaries the finalised tier used.
impl<B, M> PoolActivationSource for StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Send + Sync + 'static,
{
    fn pool_activations(&self) -> PoolActivations {
        self.activations
    }
}

impl<B, M> StoreSnapshot<B, M>
where
    B: Backend + 'static,
{
    /// A reader over the pinned backend.
    fn treestate_reader(&self) -> Result<B::Reader, TreestateReadError> {
        self.backend
            .reader()
            .map_err(|e| TreestateReadError::Fatal(format!("open reader: {e}")))
    }
}

/// Refuse a treestate read whose backing namespaces are still incomplete.
fn treestate_gate<B: Backend + 'static>(reader: &B::Reader) -> Result<(), TreestateReadError> {
    gate::<B>(
        reader,
        &[tree_state::codec::ID.into(), headers::ID.into()],
        Capability::Treestate,
    )
}

/// Refuse a subtree-roots read whose backing namespaces are still incomplete.
fn subtree_roots_gate<B: Backend + 'static>(reader: &B::Reader) -> Result<(), TreestateReadError> {
    gate::<B>(
        reader,
        &[
            <SaplingSubtreesIndex as IndexDef>::NAME.into(),
            <OrchardSubtreesIndex as IndexDef>::NAME.into(),
            <IronwoodSubtreesIndex as IndexDef>::NAME.into(),
        ],
        Capability::SubtreeRoots,
    )
}

/// Shared readiness gate: `NotServiceable(capability)` when any namespace is
/// still incomplete, mapping a probe failure to a transient error.
fn gate<B: Backend + 'static>(
    reader: &B::Reader,
    namespaces: &[Namespace],
    capability: Capability,
) -> Result<(), TreestateReadError> {
    if let Some(capability) = serviceability_gate::<B>(reader, namespaces, capability)
        .map_err(|e| TreestateReadError::Transient(format!("treestate readiness: {e}")))?
    {
        return Err(TreestateReadError::NotServiceable(capability));
    }
    Ok(())
}

/// Scan a pool's subtree namespace from `start_index`, decoding at most `limit`
/// entries (all of them when `limit` is `None`) into domain [`SubtreeRoot`]s in
/// ascending subtree-index order.
fn scan_subtrees<C, B>(
    reader: &B::Reader,
    start_index: u16,
    limit: Option<u16>,
) -> Result<Vec<SubtreeRoot>, TreestateReadError>
where
    C: EntryCodec<Key = u32, Value = zaino_indexes::indexes::subtrees::codec::SubtreeRoot>
        + IndexDef,
    B: Backend,
{
    let namespace: Namespace = <C as IndexDef>::NAME.into();
    let start = encode_key::<C>(&u32::from(start_index));
    // The key is a big-endian `u32`, so a 5-byte sentinel is above every key: an
    // open-ended scan the caller's `limit` bounds.
    let end: [u8; 5] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let cap = limit.map(usize::from);

    let mut roots: Vec<SubtreeRoot> = Vec::new();
    let mut decode_error: Option<TreestateReadError> = None;
    let mut visit = |_key: &[u8], value: &[u8]| -> ControlFlow<()> {
        match decode_value::<C>(value) {
            Ok(entry) => {
                roots.push(SubtreeRoot {
                    root: TreeRoot::from(entry.root),
                    end_height: match height_of(entry.completing_height) {
                        Ok(height) => height,
                        Err(error) => {
                            decode_error = Some(error);
                            return ControlFlow::Break(());
                        }
                    },
                });
                if cap.is_some_and(|cap| roots.len() >= cap) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
            Err(e) => {
                decode_error = Some(TreestateReadError::Fatal(format!(
                    "decode {}: {e}",
                    namespace.as_str()
                )));
                ControlFlow::Break(())
            }
        }
    };
    reader
        .scan_range(namespace, &start, &end, &mut visit)
        .map_err(|e| TreestateReadError::Transient(format!("scan {}: {e}", namespace.as_str())))?;
    if let Some(error) = decode_error {
        return Err(error);
    }
    Ok(roots)
}

/// The indexed completing height as the domain [`Height`], which is narrower.
fn height_of(height: zaino_sync::primitives::BlockHeight) -> Result<Height, TreestateReadError> {
    u32::try_from(height.value())
        .ok()
        .and_then(|h| Height::try_from(h).ok())
        .ok_or_else(|| {
            TreestateReadError::Fatal(
                "a subtree completing height exceeds the protocol limit".to_owned(),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
    use zaino_indexes::index_set::IndexSet;
    use zaino_indexes::indexes::headers::{self, HeaderValue, HeadersIndex};
    use zaino_indexes::indexes::subtrees::codec::SubtreeRoot as SubtreeRecord;
    use zaino_indexes::indexes::subtrees::SaplingSubtreesIndex;
    use zaino_indexes::indexes::tree_state::{self, TreeStateCtx, TreeStateIndex, TreeStateValue};
    use zaino_indexes::sets::light_wallet_local::LightWalletLocal;
    use zaino_persistence::{Backend, BackendWriter, NamespaceSpec, WriteOp};
    use zaino_persistence_codec::{put, reserved_namespaces, version_stamp, watermark};
    use zaino_primitives::types::{
        BlockHash, CompactDifficulty, Height, NoteCommitment, PoolActivations, ShieldedPool,
    };
    use zaino_service::error::TreestateReadError;
    use zaino_service::{Capability, TakeSnapshot, TreestateRead};
    use zaino_sync::primitives::BlockHeight;
    use zaino_sync::traits::{CumulativeAppend, ExtractCumulative, IndexDef};

    use crate::{StoreReader, StoreSnapshot};

    /// A canonical Sapling note commitment from a seed in its low 8 bytes.
    fn commitment(seed: u64) -> NoteCommitment {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        NoteCommitment::from(bytes)
    }

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    fn header(h: u32) -> HeaderValue {
        HeaderValue {
            hash: BlockHash::from([h as u8; 32]),
            prev_hash: BlockHash::from([h.wrapping_sub(1) as u8; 32]),
            time: 1_000 + h,
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
        }
    }

    /// A tree-state context at `h` adding `sapling` Sapling notes and no
    /// Orchard/Ironwood notes — so Orchard and Ironwood stay empty (the
    /// below-activation / no-notes case) at every height.
    fn ctx(h: u32, sapling: u64) -> TreeStateCtx {
        TreeStateCtx {
            height: BlockHeight::new(u64::from(h)),
            sapling_cmus: (0..sapling)
                .map(|i| commitment(u64::from(h) * 100 + i))
                .collect(),
            orchard_cmxs: Vec::new(),
            ironwood_cmxs: Vec::new(),
        }
    }

    fn specs() -> Vec<NamespaceSpec> {
        LightWalletLocal::pipelines()
            .namespace_specs()
            .into_iter()
            .chain(reserved_namespaces().map(NamespaceSpec::meta))
            .collect()
    }

    fn open_at(path: &std::path::Path) -> LmdbBackend {
        LmdbBackend::open(LmdbConfig {
            path: path.to_path_buf(),
            map_size_bytes: 16 << 20,
            namespaces: specs(),
        })
        .expect("open lmdb store")
    }

    /// Build tree-state frontiers for heights `1..=3` (two Sapling notes each),
    /// their headers, three Sapling subtree roots, and a watermark at 3.
    fn store_ops() -> (Vec<WriteOp>, Vec<TreeStateValue>) {
        let mut ops = vec![
            version_stamp::<HeadersIndex>(headers::ID.into()),
            version_stamp::<TreeStateIndex>(tree_state::codec::ID.into()),
            version_stamp::<SaplingSubtreesIndex>(<SaplingSubtreesIndex as IndexDef>::NAME.into()),
        ];

        // Chain the cumulative frontier across heights 1..=3.
        let mut carry = TreeStateIndex::initial_carry();
        let mut values = Vec::new();
        for h in 1..=3u32 {
            let entry = TreeStateIndex::extract(&ctx(h, 2), &carry).expect("extract");
            carry = entry.value.clone();
            ops.push(put::<TreeStateIndex>(
                tree_state::codec::ID.into(),
                &BlockHeight::new(u64::from(h)),
                &entry.value,
            ));
            ops.push(put::<HeadersIndex>(
                headers::ID.into(),
                &BlockHeight::new(u64::from(h)),
                &header(h),
            ));
            values.push(entry.value);
        }

        // Three Sapling subtree roots at ascending indices.
        for index in 0u32..3 {
            ops.push(put::<SaplingSubtreesIndex>(
                <SaplingSubtreesIndex as IndexDef>::NAME.into(),
                &index,
                &SubtreeRecord {
                    root: [index as u8 + 1; 32],
                    completing_height: BlockHeight::new(u64::from(index) + 1),
                },
            ));
        }

        ops.push(watermark::stamp(height(3)));
        (ops, values)
    }

    /// Sapling scheduled from height 1 (so the fixture's heights are all active);
    /// Orchard and Ironwood unscheduled, so they stay absent — the empty-pool case.
    fn activations() -> PoolActivations {
        PoolActivations {
            sapling: Some(height(1)),
            orchard: None,
            ironwood: None,
        }
    }

    async fn snapshot(backend: Arc<LmdbBackend>) -> StoreSnapshot<LmdbBackend, LightWalletLocal> {
        StoreReader::<_, LightWalletLocal>::with_activations(backend, activations())
            .snapshot()
            .await
            .expect("snapshot")
    }

    fn built() -> (tempfile::TempDir, Vec<TreeStateValue>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = open_at(dir.path());
        let (ops, values) = store_ops();
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
        (dir, values)
    }

    /// An exact-height read returns the frontier stored at that height — the
    /// Sapling pool is present, and distinct heights carry distinct frontiers.
    #[tokio::test]
    async fn treestate_reads_the_exact_height() {
        let (dir, values) = built();
        let backend = Arc::new(open_at(dir.path()));
        let snap = snapshot(backend).await;

        let at2 = snap
            .treestate(height(2))
            .await
            .expect("height 2 is covered");
        let at3 = snap
            .treestate(height(3))
            .await
            .expect("height 3 is covered");

        // The block identity is the one at the requested height.
        assert_eq!(at2.height, height(2));
        assert_eq!(at2.block_hash, BlockHash::from([2u8; 32]));
        assert_eq!(at2.time, 1_002);

        // Each height's Sapling tree is exactly the one the index stored there,
        // and they differ height to height (each block adds notes).
        let expected2 = values[1].pool_treestates(&activations(), height(2)).0;
        let expected3 = values[2].pool_treestates(&activations(), height(3)).0;
        assert_eq!(at2.sapling, expected2);
        assert_eq!(at3.sapling, expected3);
        assert_ne!(
            at2.sapling, at3.sapling,
            "distinct heights, distinct frontiers"
        );
    }

    /// A pool with no notes at the height is reported `None` (the `""` the wire
    /// renders), not an empty tree. Sapling is present; Orchard and Ironwood,
    /// which this fixture never funds, are absent.
    #[tokio::test]
    async fn an_empty_pool_is_absent() {
        let (dir, _values) = built();
        let snap = snapshot(Arc::new(open_at(dir.path()))).await;
        let at = snap.treestate(height(3)).await.expect("covered");
        assert!(at.sapling.is_some(), "Sapling has notes");
        assert_eq!(at.orchard, None, "Orchard funded nothing → absent");
        assert_eq!(at.ironwood, None, "Ironwood funded nothing → absent");
    }

    /// A height above the finalised coverage is `NotServiceable`, never an empty
    /// tree: the store's coverage is exactly its watermark.
    #[tokio::test]
    async fn above_coverage_is_not_serviceable() {
        let (dir, _values) = built();
        let snap = snapshot(Arc::new(open_at(dir.path()))).await;
        assert!(matches!(
            snap.treestate(height(4)).await,
            Err(TreestateReadError::NotServiceable(Capability::Treestate))
        ));
    }

    /// Subtree roots scan in ascending index order, honour `start_index` and
    /// `limit`, and are per pool (an unfunded pool returns nothing).
    #[tokio::test]
    async fn subtree_roots_scan_paged_and_per_pool() {
        let (dir, _values) = built();
        let snap = snapshot(Arc::new(open_at(dir.path()))).await;

        let all = snap
            .subtree_roots(ShieldedPool::Sapling, 0, None)
            .await
            .expect("sapling subtree roots");
        assert_eq!(all.len(), 3);
        assert_eq!(<[u8; 32]>::from(all[0].root)[0], 1);
        assert_eq!(all[0].end_height, height(1));
        assert_eq!(<[u8; 32]>::from(all[2].root)[0], 3);

        // Paged: start at index 1, take one.
        let page = snap
            .subtree_roots(ShieldedPool::Sapling, 1, Some(1))
            .await
            .expect("page");
        assert_eq!(page.len(), 1);
        assert_eq!(<[u8; 32]>::from(page[0].root)[0], 2);

        // Orchard was never funded, so its subtree namespace is empty.
        let orchard = snap
            .subtree_roots(ShieldedPool::Orchard, 0, None)
            .await
            .expect("orchard subtree roots");
        assert!(orchard.is_empty());
    }
}

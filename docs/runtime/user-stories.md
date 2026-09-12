# Driving-Port User Stories

Living document — updated as development uncovers modifications or new stories.
Layered by actor. Each story states what the consumer wants, and is traced to the
**capability** (a `zaino-service` trait) and the **component** that serves it. A
story is the *why* behind a capability; no capability lands without one.

Status key: ✅ served (impl + test) · 🟡 scaffolded (trait exists) · ⬜ not started.

---

## Actors

- **Full-wallet library** — zallet and future in-process embedders. Links Zaino,
  owns its own parsing/wallet-state.
- **Lightwallet-serving clients** — mobile/light wallets via zainod's gRPC
  `CompactTxStreamer`.
- **Full-node-RPC / explorer** — via zainod's JSON-RPC.
- **Operator** — deploys and monitors.

---

## Level 0 — Cross-cutting (every consumer)

> **US-0.1 — Snapshot coherence.** As any consumer, I want to pin a view of the
> chain and have every read through it stay consistent across reorgs for the
> life of the handle, so a multi-read request never sees torn/mixed-chain data.
> — capability: `TakeSnapshot`/`Snapshot`; component: `RuntimeSnapshot` (pinned
> NFS `Arc<Chain>` + FS watermark). ADR-0003. 🟡

> **US-0.2 — Serviceability during catch-up.** As any consumer, I want the port
> to tell me which capabilities are answerable *now*, and to serve what it can
> while still syncing (finalised reads before the recent window exists), so I get
> partial service instead of an all-or-nothing boot.
> — capability: `Serviceable`; component: `NfsView` readiness + serviceability
> manifest. Recent reads while `Syncing` → `NotServiceable`, never a false miss.
> ✅ (readiness path + manifest derivation tested — `Runtime: Serviceable`
> projects finalised watermark + NFS readiness + config per capability)

> **US-0.3 — Reorg safety.** As any consumer, reads must never return data for a
> chain the snapshot isn't on; a reorg during my request must not corrupt my view.
> — component: NFS `Chain` + `find_trim_index` (Lean-verified) + snapshot pin. 🟡

---

## Level 1 — Full-wallet library (zallet)

> **US-1.1 — Coherent block stream.** As a full-wallet lib, I want to stream
> blocks over a height range, coherent to one snapshot, so a scan isn't torn by a
> mid-scan reorg. — `CompactBlockRead::stream_compact` / passthrough. 🟡

> **US-1.2 — Tip-change subscription.** As a full-wallet lib, I want an explicit
> tip-change signal, so I learn of new blocks without scraping a mempool-close
> side effect. — `TipSubscribe`. 🟡

> **US-1.3 — Coherent address unspent set.** As a full-wallet lib, I want an
> address's unspent outpoints as of a snapshot, correct **across the
> finalised/recent boundary** — finalised UTXOs minus those spent in the recent
> window, plus UTXOs created in the recent window still unspent — so transparent
> balance reconciles without a seam. — `AddressRead::unspent_outpoints`;
> component: **a merge of FS index ∪ NFS re-derivation**. ⬜ *(next)*

> **US-1.4 — Spend status.** As a full-wallet lib, I want the spend status of an
> outpoint as of a snapshot. — `SpendRead::spend_status`. 🟡

> **US-1.5 — Treestate / witnesses.** As a full-wallet lib, I want treestate and
> subtree roots at an in-view height, to build note witnesses. — `TreestateRead`. 🟡

> **US-1.6 — Broadcast.** As a full-wallet lib, I want to broadcast a tx and get
> a txid or a typed rejection. — `Broadcast` (validator passthrough). 🟡

> **US-1.7 — Full blocks / raw transactions.** As a full-wallet lib, I want full
> `Block`/raw-tx bytes to parse into my own types. — served by **validator
> passthrough, by hash within the snapshot** (Q4). ⬜

> **US-1.8 — Fork point.** As a full-wallet lib, I want to locate the fork point
> between my view and the best chain via a locator, to rewind exactly on reorg.
> — `ForkReconcile::fork_point`. 🟡

---

## Level 2 — Lightwallet serving (via zainod gRPC)

> **US-2.1** compact-block stream over a range. — `CompactBlockRead`. 🟡
> **US-2.2** treestate + subtree roots. — `TreestateRead`. 🟡
> **US-2.3** address txids / balance / utxos. — `AddressRead`. ⬜
> **US-2.4** tip / mempool / broadcast. — `TipSubscribe`/`MempoolSubscribe`/`Broadcast`. 🟡

---

## Level 3 — Full-node-RPC / explorer (via zainod JSON-RPC)

> **US-3.1** verbose block/tx by height or hash. — passthrough / verbose projection. ⬜
> **US-3.2** address balance / deltas / utxos / txids over ranges. — `AddressRead`. ⬜
> **US-3.3** chain info / stats. — (RPC surface). ⬜
> **US-3.4** mempool contents + broadcast. — `MempoolSubscribe`/`Broadcast`. 🟡

---

## Level 4 — Operator

> **US-4.1 — Clean resume.** As an operator, I want to kill and restart and have
> sync resume from the last committed height with no manual step. — FS watermark
> + freeze commit; NFS re-established on boot. ⬜

> **US-4.2 — Observable, private sync progress.** As an operator, I want sync
> progress observable behind a **non-default** feature flag (per the privacy
> policy). — feature-gated tracing/metrics. ⬜

---

## Traceability

The composition (`composition.md`) exists to serve these stories: `RuntimeSnapshot`
routing serves US-0.1/1.1/2.1; `NfsView` readiness serves US-0.2; the NFS reorg
window serves US-0.3/1.8; the FS index engine serves the finalised half of every
read; validator passthrough serves US-1.7/3.1. The address **merge** (US-1.3/2.3/
3.2) is the next capability, and it drives adding address re-derivation to
`NfsSnapshot`.

## Changelog

| Date | Change |
|------|--------|
| 2026-07-22 | Initial draft. Actors + Levels 0–4, traced to capabilities. |
| 2026-07-24 | US-1.7 served (passthrough). FS/NFS decomposed into spine + per-capability traits. US-0.2 manifest derivation landed (`Runtime: Serviceable`). |

/-
  Fuel analysis: find_trim_index walk distance

  After forward fill: cs = rt - D + 1.  Walking from rt down to cs
  takes D steps.  trim_from = cs means the fork is at the freezer
  boundary — the block at cs - 1 (in LMDB) is the common ancestor.

  The theorem `findTrimIndex_cs_le_trim_from` below states that the
  walk from the remote tip always reaches cs before exhausting fuel,
  so trim_from ≥ cs.  This follows from the fuel analysis (after
  forward fill, cs = rt - D + 1, and the walk takes at most D steps).

  find_trim_index: walk backward from remote tip, accumulate a
  fragment, and stop at the common ancestor.

  The function stops when it finds a **common point** — a height
  where the remote block builds on top of our local data
  (chain[h-1] or freezer[cs-1] or genesis).  It does NOT verify
  the internal contiguity of the accumulated fragment; the caller
  uses `List.IsChain` for that.

  The Rust implementation at
  packages/zaino-store/src/sync.rs :: find_trim_index is the
  reference.
-/

import Proof.Realization

set_option linter.unusedVariables false

/-- Optional list indexing (List.get? is not available in this Std version). -/
private def List.get? (l : List α) (i : Nat) : Option α :=
  if h : i < l.length then some (l.get ⟨i, h⟩) else none

/-- Block data: every block carries a hash and a prev_hash. -/
structure Block (α : Type) where
  hash     : α
  prevHash : α

inductive FindTrimError where
  | fuelExhausted    -- fuel = 0 before finding the common ancestor
  | chainIncoherent  -- bad prev_hash at the genesis boundary
  deriving Repr

/-- Find the common ancestor between the remote chain and our local
    chain by walking backward from `h`.  For each height:

      * fetch the remote block at height `h`,
      * prepend it to `acc`,
      * check whether it builds on top of our local block at `h-1`.

    If the check succeeds (`prev_hash` matches), `h` is the first
    height to replace — return it together with the fragment.

    At the freezer boundary (`h = cs`) the expected previous block
    is the last freezer block (or genesis when `cs = 0`).  A
    mismatch at the boundary is `chainIncoherent`.

    The function does NOT validate that successive elements of `acc`
    link to each other — the caller should verify
    `List.IsChain R acc` separately (note: `acc` is in low-to-high
    order since each step prepends a lower-height block).

    Termination is proved structurally on `fuel`. -/
def findTrimIndexInt [DecidableEq α] (h cs : Nat) (freezer chain : List (Block α))
    (fetchRemote : Nat → Block α) (acc : List (Block α)) (fuel : Nat)
    (genesisHash : α) : Except FindTrimError (Nat × List (Block α)) :=
  match fuel with
  | 0 => .error .fuelExhausted
  | fuel' + 1 =>
    let tip := fetchRemote h
    let acc' := tip :: acc

    if h = cs then
      -- At the freezer / chain boundary.
      if cs = 0 then
        -- Genesis: the remote tip's prevHash must be the genesis hash.
        if tip.prevHash ≠ genesisHash then
          .error .chainIncoherent
        else
          .ok (cs, acc')
      else
        -- The expected previous block is the last freezer block.
        match freezer.get? (cs - 1) with
        | none => .error .chainIncoherent
        | some fb =>
          if tip.prevHash ≠ fb.hash then
            .error .fuelExhausted
          else
            .ok (cs, acc')
    else
      -- h > cs: look up the local block at height h-1 in the chain.
      -- chain is 0-indexed from cs, so chain[h - cs - 1] = height h-1.
      let chainIdx := h - cs - 1
      match chain.get? chainIdx with
      | none =>
        -- h-1 is beyond our chain tip.  Keep walking down.
        findTrimIndexInt (h - 1) cs freezer chain fetchRemote acc' fuel' genesisHash
      | some cb =>
        if tip.prevHash = cb.hash then
          -- Common ancestor found: remote block at h builds on our
          -- local block at h-1.  trim_from = h.
          .ok (h, acc')
        else
          findTrimIndexInt (h - 1) cs freezer chain fetchRemote acc' fuel' genesisHash

/-- Entry point: start at `rtip`, walk down with fuel = D
    (MAX_REORG_DEPTH = 101) and an empty accumulator. -/
def findTrimIndex [DecidableEq α] (cs rtip : Nat) (freezer chain : List (Block α))
    (fetchRemote : Nat → Block α) (genesisHash : α) :
    Except FindTrimError (Nat × List (Block α)) :=
  findTrimIndexInt rtip cs freezer chain fetchRemote [] D genesisHash

----------------------------------------------------------------------
-- Chain preservation:  (freezer ++ chain).take trim_from ++ fragment
-- is IsChain when findTrimIndex finds the anchor.
--
-- The existing theorem `addFragment_full_chain` already proves this
-- for any α and R.  We instantiate it for Block's linking relation.
----------------------------------------------------------------------

/-- The "links-to" relation for Block: child.prevHash = parent.hash. -/
def Block.links (parent child : Block α) : Prop :=
  child.prevHash = parent.hash

theorem findTrimIndex_chain [DecidableEq α]
    (freezer chain fragment : List (Block α)) (trim_from : Nat)
    (h_local : List.IsChain Block.links (freezer ++ chain))
    (h_frag : List.IsChain Block.links fragment)
    (h_link : ∀ x ∈ ((freezer ++ chain).take trim_from).getLast?,
              ∀ y ∈ fragment.head?, Block.links x y) :
    List.IsChain Block.links (((freezer ++ chain).take trim_from) ++ fragment) :=
  addFragment_full_chain freezer chain fragment h_local h_frag trim_from h_link

/-- Bridge: a successful `findTrimIndex` together with a valid
    `Realization` and a well-formed fragment produces a new
    `Realization` for the post-`addFragment` state.

    The preconditions beyond `h_success` include `h_ge_cs : s.cs ≤ trim_from`,
    which is now derivable from the algorithm's success via
    `findTrimIndex_cs_le_trim_from`. -/
def findTrimIndex_realization [DecidableEq β]
    (s : State) (r : Realization (Block β) Block.links s)
    (rtip : Nat) (fetchRemote : Nat → Block β) (genesisHash : β)
    (trim_from : Nat) (fragment : List (Block β))
    (h_success : findTrimIndex s.cs rtip r.freezer r.chain fetchRemote genesisHash
                   = .ok (trim_from, fragment))
    (h_frag_chain : List.IsChain Block.links fragment)
    (h_trim : trim_from ≤ s.ct)
    (h_ge_cs : s.cs ≤ trim_from)
    (hf : fragment.length ≤ D)
    (h_link : ∀ x ∈ ((r.freezer ++ r.chain).take trim_from).getLast?,
              ∀ y ∈ fragment.head?, Block.links x y) :
    Realization (Block β) Block.links (addFragment s trim_from fragment.length h_trim hf) :=
  r.addFragment_realization fragment h_frag_chain trim_from h_trim h_ge_cs hf h_link

/-- The `trim_from` returned by `findTrimIndex` is never below `cs`.

    The function only ever returns `(cs, _)` (boundary case) or `(h, _)`
    for some height `h ≥ cs` visited during the walk.  The initial height
    `rtip` is assumed ≥ `cs` (the remote tip is never behind the local
    freezer boundary in the sync protocol). -/
theorem findTrimIndex_cs_le_trim_from [DecidableEq α]
    (cs rtip : Nat) (freezer chain : List (Block α))
    (fetchRemote : Nat → Block α) (genesisHash : α)
    (trim_from : Nat) (fragment : List (Block α))
    (h_success : findTrimIndex cs rtip freezer chain fetchRemote genesisHash
                   = .ok (trim_from, fragment))
    (h_rtip_ge_cs : cs ≤ rtip) :
    cs ≤ trim_from :=
by
  -- The walk only ever succeeds by returning either `cs` (at the freezer/
  -- genesis boundary) or the current height `h` (when the common ancestor is
  -- found).  Every height it visits stays ≥ cs, so the returned index is too.
  -- We prove this invariant for the worker by induction on the fuel; in each
  -- step `split` enumerates the branches, the error branches contradict the
  -- `.ok` hypothesis, the two return branches give `trim_from = cs` or
  -- `trim_from = h`, and the recursive branches feed `h - 1 ≥ cs` to `ih`.
  unfold findTrimIndex at h_success
  suffices H : ∀ (fuel h : Nat) (acc : List (Block α)), cs ≤ h →
      findTrimIndexInt h cs freezer chain fetchRemote acc fuel genesisHash
        = .ok (trim_from, fragment) → cs ≤ trim_from by
    exact H D rtip [] h_rtip_ge_cs h_success
  intro fuel
  induction fuel with
  | zero => intro h acc _ hw; simp [findTrimIndexInt] at hw
  | succ fuel' ih =>
    intro h acc h_ge hw
    unfold findTrimIndexInt at hw
    simp only [] at hw
    split at hw <;> (try split at hw) <;> (try split at hw) <;> (try split at hw) <;>
      first
        | exact ih _ _ (by omega) hw
        | simp_all

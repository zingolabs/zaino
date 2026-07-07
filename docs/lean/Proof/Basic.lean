/-
  Sync algorithm — size, connectivity, and chain invariants (Lean 4).

  We model the chain abstractly: cs (freezer size), ct (next height),
  cl (chain length = ct - cs).  All IO is mocked.

  D = MAX_REORG_DEPTH = 101.

  trim_from is the first height to replace (inclusive).  The chain keeps
  [cs, trim_from - 1] and the fragment replaces [trim_from, ct).
  trim_from = cs means the fork is at the freezer boundary — the block
  at cs - 1 (in LMDB) is the common ancestor.

  The `List.IsChain` formalization proves that freezer ++ chain
  maintains contiguity under an abstract link relation R.  Lists are
  not materialized in State; theorems are parametrized by lists that
  realize the numeric state.

  This module contains the basic definitions: the constant `D`, the
  numeric `State`, the `ChainFragment` structure, and the core
  invariant lemmas.
-/

import Mathlib.Data.List.Chain
import Mathlib.Data.List.TakeDrop

set_option linter.unusedVariables false

def D : Nat := 101

----------------------------------------------------------------------
-- State (numeric — no lists stored)
----------------------------------------------------------------------

structure State where
  cs    : Nat
  ct    : Nat
  cl    : Nat
  h_inv : ct = cs + cl

----------------------------------------------------------------------
-- ChainFragment: a list with an IsChain proof
----------------------------------------------------------------------

/-- A fragment of the chain: a list of blocks together with a proof
    that consecutive elements satisfy `R`. -/
structure ChainFragment (α : Type) (R : α → α → Prop) where
  blocks  : List α
  h_chain : List.IsChain R blocks

----------------------------------------------------------------------
-- Invariant: ct = cs + cl  (definitional)
----------------------------------------------------------------------

theorem ct_sub_cs_eq_cl (s : State) : s.ct - s.cs = s.cl := by
  rw [s.h_inv, Nat.add_sub_cancel_left]

theorem cs_eq_ct_iff_cl_zero (s : State) : s.cs = s.ct ↔ s.cl = 0 := by
  rw [s.h_inv]; constructor <;> omega

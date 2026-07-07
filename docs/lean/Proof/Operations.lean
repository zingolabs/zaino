/-
  State operations and their size invariants.

  Defines the abstract operations on `State` (`openState`, `flush`,
  `appendFreezer`, `appendChain`, `trim`, `trimToAnchor`,
  `addFragment`) together with the size/bound theorems they satisfy
  and the `sync_step` post-conditions.
-/

import Proof.Basic

set_option linter.unusedVariables false

----------------------------------------------------------------------
-- open
----------------------------------------------------------------------

def openState (cs₀ : Nat) : State :=
  { cs := cs₀, ct := cs₀, cl := 0, h_inv := by omega }

----------------------------------------------------------------------
-- flush_chain_to_lmdb
--   cs' = cs + cl,  ct' = cs',  cl' = 0
----------------------------------------------------------------------

def flush (s : State) : State :=
  { cs := s.cs + s.cl, ct := s.cs + s.cl, cl := 0, h_inv := by omega }

theorem flush_cl_zero (s : State) : (flush s).cl = 0 := rfl
theorem flush_cs_eq_ct (s : State) : (flush s).cs = (flush s).ct := rfl
theorem flush_cs_advances (s : State) : (flush s).cs = s.cs + s.cl := rfl

----------------------------------------------------------------------
-- append_to_freezer (n blocks, chain must be empty)
--   cs' = cs + n,  ct' = cs',  cl' = 0
----------------------------------------------------------------------

def appendFreezer (s : State) (n : Nat) (_ : s.cl = 0) : State :=
  { cs := s.cs + n, ct := s.cs + n, cl := 0, h_inv := by omega }

theorem appendFreezer_cl_zero (s : State) (n : Nat) (h : s.cl = 0) :
    (appendFreezer s n h).cl = 0 := rfl

theorem appendFreezer_cs_advances (s : State) (n : Nat) (h : s.cl = 0) :
    (appendFreezer s n h).cs = s.cs + n := rfl

----------------------------------------------------------------------
-- append_to_chain (fragment of length flen ≤ D)
--   new_cl = old_cl + flen  ≤  2·D  (when old_cl ≤ D)
----------------------------------------------------------------------

def appendChain (s : State) (flen : Nat) (_ : flen ≤ D) : State :=
  { cs := s.cs, ct := s.cs + (s.cl + flen), cl := s.cl + flen, h_inv := rfl }

theorem appendChain_cl_bounded (s : State) (flen : Nat) (hf : flen ≤ D)
    (h_prior : s.cl ≤ D) : (appendChain s flen hf).cl ≤ 2 * D := by
  unfold appendChain; simp
  have hsum : s.cl + flen ≤ D + D := Nat.add_le_add h_prior hf
  omega

----------------------------------------------------------------------
-- trim_chain
--   If cl > D: cs' = cs + (cl - D),  cl' = D,  ct unchanged
--   Post: cl' ≤ D
----------------------------------------------------------------------

def trim (s : State) : State :=
  if h : s.cl > D then
    let c := s.cl - D
    { cs := s.cs + c, ct := s.ct, cl := s.cl - c,
      h_inv := by rw [s.h_inv]; omega }
  else s

theorem trim_cl_bounded (s : State) : (trim s).cl ≤ D := by
  unfold trim
  by_cases h : s.cl > D
  · simp [h]; omega
  · simp [h]; exact Nat.le_of_not_gt h

theorem trim_ct_unchanged (s : State) : (trim s).ct = s.ct := by
  unfold trim; split <;> rfl

----------------------------------------------------------------------
-- trimToAnchor: keep [cs, trim_from - 1], discard [trim_from, ct).
--   trim_from is the first height to replace (inclusive).
--   Pre: trim_from ≤ ct
--   Post: cs unchanged,
--         ct' = cs         if trim_from ≤ cs (boundary: empty chain)
--         ct' = trim_from  if trim_from > cs (anchor in chain)
----------------------------------------------------------------------

def trimToAnchor (s : State) (trim_from : Nat) (_ : trim_from ≤ s.ct) : State :=
  if h : trim_from ≤ s.cs then
    -- Boundary or below: empty the chain.  The freezer block at
    -- cs - 1 is the common ancestor (verified by find_trim_index).
    -- When cs = 0 that ancestor is genesis.
    { cs := s.cs, ct := s.cs, cl := 0, h_inv := by omega }
  else
    -- Anchor in chain: keep [cs, trim_from - 1].
    { cs := s.cs
    , ct := trim_from
    , cl := trim_from - s.cs
    , h_inv := by omega
    }

theorem trimToAnchor_cs_unchanged (s : State) (trim_from : Nat)
    (h_trim : trim_from ≤ s.ct) :
    (trimToAnchor s trim_from h_trim).cs = s.cs := by
  unfold trimToAnchor; split <;> rfl

theorem trimToAnchor_ct (s : State) (trim_from : Nat)
    (h_trim : trim_from ≤ s.ct) :
    (trimToAnchor s trim_from h_trim).ct =
    if trim_from ≤ s.cs then s.cs else trim_from := by
  unfold trimToAnchor; split <;> rfl

theorem trimToAnchor_cl (s : State) (trim_from : Nat)
    (h_trim : trim_from ≤ s.ct) :
    (trimToAnchor s trim_from h_trim).cl =
    if trim_from ≤ s.cs then 0 else trim_from - s.cs := by
  unfold trimToAnchor; split <;> rfl

theorem trimToAnchor_cl_bounded (s : State) (trim_from : Nat)
    (h_trim : trim_from ≤ s.ct) :
    (trimToAnchor s trim_from h_trim).cl ≤ s.cl := by
  unfold trimToAnchor; split
  · simp
  · rename_i h_not_le
    simp
    rw [s.h_inv] at h_trim
    omega

----------------------------------------------------------------------
-- add_fragment: trim to trim_from, then append fragment.
--   trim_from is the first height to replace (inclusive).
--   Pre: trim_from ≤ ct.  The caller (find_trim_index) also
--   guarantees cs ≤ trim_from, but the state model works without it.
----------------------------------------------------------------------

def addFragment (s : State) (trim_from : Nat) (flen : Nat)
    (h_trim : trim_from ≤ s.ct) (hf : flen ≤ D) : State :=
  let s1 := trimToAnchor s trim_from h_trim
  appendChain s1 flen hf

theorem addFragment_cs_unchanged (s : State) (trim_from : Nat) (flen : Nat)
    (h_trim : trim_from ≤ s.ct) (hf : flen ≤ D) :
    (addFragment s trim_from flen h_trim hf).cs = s.cs := by
  unfold addFragment appendChain; simp
  exact trimToAnchor_cs_unchanged s trim_from h_trim

theorem addFragment_ct (s : State) (trim_from : Nat) (flen : Nat)
    (h_trim : trim_from ≤ s.ct) (hf : flen ≤ D) :
    (addFragment s trim_from flen h_trim hf).ct =
    (if trim_from ≤ s.cs then s.cs else trim_from) + flen := by
  unfold addFragment appendChain trimToAnchor
  split
  · simp
  · simp; omega

theorem addFragment_cl_bounded (s : State) (trim_from : Nat) (flen : Nat)
    (h_trim : trim_from ≤ s.ct) (hf : flen ≤ D)
    (h_prior : s.cl ≤ D) : (addFragment s trim_from flen h_trim hf).cl ≤ 2 * D := by
  unfold addFragment
  have h_trim_cl : (trimToAnchor s trim_from h_trim).cl ≤ D := by
    have h_le : (trimToAnchor s trim_from h_trim).cl ≤ s.cl :=
      trimToAnchor_cl_bounded s trim_from h_trim
    omega
  have h_append := appendChain_cl_bounded (trimToAnchor s trim_from h_trim) flen hf h_trim_cl
  exact h_append

----------------------------------------------------------------------
-- sync_step post-conditions
----------------------------------------------------------------------

theorem sync_step_size (s : State) : (trim s).ct - (trim s).cs = (trim s).cl := by
  rw [(trim s).h_inv, Nat.add_sub_cancel_left]

theorem sync_step_bounded (s : State) : (trim s).cl ≤ D := trim_cl_bounded s

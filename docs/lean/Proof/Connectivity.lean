/-
  Connectivity: freezer ++ chain forms a contiguous range [cs, ct).

  Each operation preserves the `connected` predicate (cs ≤ ct).
-/

import Proof.Operations

set_option linter.unusedVariables false

----------------------------------------------------------------------
-- Connectivity: freezer ++ chain forms contiguous [0, ct)
----------------------------------------------------------------------

def connected (s : State) : Prop := s.cs ≤ s.ct

theorem connected_holds (s : State) : connected s := by
  unfold connected; rw [s.h_inv]; omega

theorem flush_connected (s : State) : connected (flush s) := by
  unfold connected flush; simp

theorem appendFreezer_connected (s : State) (n : Nat) (h : s.cl = 0) :
    connected (appendFreezer s n h) := by
  unfold connected appendFreezer; simp

theorem appendChain_connected (s : State) (flen : Nat) (hf : flen ≤ D) :
    connected (appendChain s flen hf) := by
  unfold connected appendChain; simp

theorem trim_connected (s : State) : connected (trim s) := by
  unfold trim; split
  · unfold connected; simp; rw [s.h_inv]; omega
  · exact connected_holds s

theorem trimToAnchor_connected (s : State) (trim_from : Nat)
    (h_trim : trim_from ≤ s.ct) :
    connected (trimToAnchor s trim_from h_trim) := by
  unfold connected trimToAnchor; split
  · simp
  · simp; omega

theorem addFragment_connected (s : State) (trim_from : Nat) (flen : Nat)
    (h_trim : trim_from ≤ s.ct) (hf : flen ≤ D) :
    connected (addFragment s trim_from flen h_trim hf) := by
  unfold addFragment
  have h_conn_s1 : connected (trimToAnchor s trim_from h_trim) :=
    trimToAnchor_connected s trim_from h_trim
  exact appendChain_connected (trimToAnchor s trim_from h_trim) flen hf

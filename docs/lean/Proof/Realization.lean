/-
  Realization: bridges numeric State to concrete lists.

  A Realization bundles the lists (data) with proofs that their
  lengths match the numeric state AND that freezer ++ chain
  satisfies IsChain R.
-/

import Proof.Operations
import Proof.IsChain

set_option linter.unusedVariables false

structure Realization (α : Type) (R : α → α → Prop) (s : State) where
  freezer : List α
  chain   : List α
  h_len_freezer : freezer.length = s.cs
  h_len_chain   : chain.length = s.cl
  h_chain       : List.IsChain R (freezer ++ chain)

namespace Realization
  variable {α : Type} {R : α → α → Prop}

  theorem full_length {s : State} (r : Realization α R s) :
      (r.freezer ++ r.chain).length = s.ct := by
    rw [List.length_append, r.h_len_freezer, r.h_len_chain, ← s.h_inv]

  /-- flush: freezer absorbs the chain, chain becomes empty. -/
  def flush_realization {s : State} (r : Realization α R s) :
      Realization α R (flush s) :=
    { freezer := r.freezer ++ r.chain
    , chain := []
    , h_len_freezer := by
        rw [List.length_append, r.h_len_freezer, r.h_len_chain]; rfl
    , h_len_chain := rfl
    , h_chain := r.h_chain.append .nil (by simp)
    }

  /-- appendChain: appends a fragment to the chain. -/
  def appendChain_realization {s : State} (r : Realization α R s)
      (fragment : List α) (h_frag : List.IsChain R fragment) (hf : fragment.length ≤ D)
      (h_link : ∀ x ∈ (r.freezer ++ r.chain).getLast?, ∀ y ∈ fragment.head?, R x y) :
      Realization α R (appendChain s fragment.length hf) :=
    { freezer := r.freezer
    , chain := r.chain ++ fragment
    , h_len_freezer := r.h_len_freezer
    , h_len_chain := by
        rw [List.length_append, r.h_len_chain]; rfl
    , h_chain := by
        rw [← List.append_assoc]
        exact r.h_chain.append h_frag h_link
    }

  /-- addFragment: trim from trim_from, append fragment.
      Requires cs ≤ trim_from (fuel invariant), which ensures
      take trim_from freezer = freezer.  The h_link connects
      the prefix (freezer when trim_from = cs, or freezer ++
      chain-prefix when trim_from > cs) to the fragment. -/
  def addFragment_realization {s : State} (r : Realization α R s)
      (fragment : List α) (h_frag : List.IsChain R fragment)
      (trim_from : Nat) (h_trim : trim_from ≤ s.ct) (h_ge_cs : s.cs ≤ trim_from)
      (hf : fragment.length ≤ D)
      (h_link : ∀ x ∈ ((r.freezer ++ r.chain).take trim_from).getLast?,
                ∀ y ∈ fragment.head?, R x y) :
      Realization α R (addFragment s trim_from fragment.length h_trim hf) :=
    let k := trim_from - s.cs
    have hk_chain : k ≤ r.chain.length := by
      unfold k; rw [r.h_len_chain]
      have : trim_from ≤ s.cs + s.cl := by rw [← s.h_inv]; exact h_trim
      omega
    have h_spec_cs : (addFragment s trim_from fragment.length h_trim hf).cs = s.cs :=
      addFragment_cs_unchanged s trim_from fragment.length h_trim hf
    have h_spec_cl : (addFragment s trim_from fragment.length h_trim hf).cl =
        k + fragment.length := by
      unfold addFragment appendChain; simp
      rw [trimToAnchor_cl s trim_from h_trim]; split <;> omega
    have h_freezer_take : List.take trim_from r.freezer = r.freezer :=
      List.take_of_length_le (l := r.freezer) (by rw [r.h_len_freezer]; exact h_ge_cs)
    have h_prefix_eq : (r.freezer ++ r.chain).take trim_from =
        r.freezer ++ r.chain.take k := by
      rw [List.take_append, h_freezer_take, r.h_len_freezer]
    { freezer := r.freezer
    , chain := r.chain.take k ++ fragment
    , h_len_freezer := by rw [h_spec_cs]; exact r.h_len_freezer
    , h_len_chain := by
        rw [List.length_append, List.length_take_of_le hk_chain, h_spec_cl]
    , h_chain := by
        rw [← List.append_assoc, ← h_prefix_eq]
        exact addFragment_full_chain r.freezer r.chain fragment
          r.h_chain h_frag trim_from h_link
    }

end Realization

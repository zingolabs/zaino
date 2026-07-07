/-
  IsChain preservation theorems.

  Each theorem shows that if lists realizing the pre-state satisfy
  `IsChain` (with the abstract link relation R), then after the
  operation the resulting lists also satisfy `IsChain`.

  The linking condition is mathlib's:
    ∀ x ∈ l₁.getLast?, ∀ y ∈ l₂.head?, R x y
  This is vacuously true when either list is empty.
-/

import Proof.Basic

set_option linter.unusedVariables false

section IsChainPreservation
  variable {α : Type} {R : α → α → Prop}

  /-- flush: freezer absorbs the chain.  The full concatenation
      becomes (freezer ++ chain) ++ [], which is trivially IsChain
      since the empty list requires no linking condition. -/
  theorem flush_full_chain (freezer chain : List α)
      (h : List.IsChain R (freezer ++ chain)) :
      List.IsChain R ((freezer ++ chain) ++ []) :=
    h.append .nil (by simp)

  /-- trim: the concatenation freezer' ++ chain' is identical to
      freezer ++ chain (the prefix of chain that moves to the freezer
      is exactly balanced by take/drop), so IsChain is preserved. -/
  theorem trim_full_chain (freezer chain : List α) (k : Nat)
      (h : List.IsChain R (freezer ++ chain)) :
      List.IsChain R ((freezer ++ chain.take k) ++ chain.drop k) := by
    have h_eq : (freezer ++ chain.take k) ++ chain.drop k = freezer ++ chain := by
      simp
    rw [h_eq]; exact h

  /-- appendFreezer: extends the freezer by a fragment when the chain
      is empty.  The linking condition ensures the fragment's first
      element relates to the freezer's last element. -/
  theorem appendFreezer_full_chain (freezer fragment : List α)
      (h_freezer : List.IsChain R freezer)
      (h_frag : List.IsChain R fragment)
      (h_link : ∀ x ∈ freezer.getLast?, ∀ y ∈ fragment.head?, R x y) :
      List.IsChain R (freezer ++ fragment) :=
    h_freezer.append h_frag h_link

  /-- appendChain: extends the chain by a fragment.
      The full concatenation becomes (freezer ++ chain) ++ fragment.
      The linking condition connects the chain tip to the fragment head. -/
  theorem appendChain_full_chain (freezer chain fragment : List α)
      (h_full : List.IsChain R (freezer ++ chain))
      (h_frag : List.IsChain R fragment)
      (h_link : ∀ x ∈ (freezer ++ chain).getLast?, ∀ y ∈ fragment.head?, R x y) :
      List.IsChain R ((freezer ++ chain) ++ fragment) :=
    h_full.append h_frag h_link

  /-- appendChain variant: freezer unchanged, chain extended.
      Useful when reasoning about freezer and chain separately. -/
  theorem appendChain_full_chain' (freezer chain fragment : List α)
      (h_full : List.IsChain R (freezer ++ chain))
      (h_frag : List.IsChain R fragment)
      (h_link : ∀ x ∈ (freezer ++ chain).getLast?, ∀ y ∈ fragment.head?, R x y) :
      List.IsChain R (freezer ++ (chain ++ fragment)) := by
    rw [← List.append_assoc]; exact h_full.append h_frag h_link

  /-- addFragment: trim the full chain from trim_from, then append the
      fragment.  The result is:
        (freezer ++ chain).take trim_from ++ fragment

      When trim_from ≤ cs the prefix is entirely within the freezer;
      when trim_from > cs the prefix includes some chain blocks.
      The h_link condition connects that prefix's last element to the
      fragment's head — whether that last element lives in the freezer
      (boundary case) or in the chain (fork-in-chain case). -/
  theorem addFragment_full_chain (freezer chain fragment : List α)
      (h_full : List.IsChain R (freezer ++ chain))
      (h_frag : List.IsChain R fragment)
      (trim_from : Nat)
      (h_link : ∀ x ∈ ((freezer ++ chain).take trim_from).getLast?,
                ∀ y ∈ fragment.head?, R x y) :
      List.IsChain R (((freezer ++ chain).take trim_from) ++ fragment) :=
    (h_full.take trim_from).append h_frag h_link

  /-- The prefix up to but excluding trim_from is IsChain. -/
  theorem addFragment_prefix_isChain (freezer chain : List α)
      (h_full : List.IsChain R (freezer ++ chain)) (trim_from : Nat) :
      List.IsChain R ((freezer ++ chain).take trim_from) :=
    h_full.take trim_from

  /-- The fragment alone is IsChain (trivial from ChainFragment). -/
  theorem addFragment_fragment_isChain (fragment : List α)
      (h_frag : List.IsChain R fragment) :
      List.IsChain R fragment := h_frag

end IsChainPreservation

/-
  Sync algorithm — size, connectivity, and chain invariants (Lean 4).

  This is the aggregating module.  The development is split into:

    * `Proof.Basic`         — `D`, `State`, `ChainFragment`, invariants
    * `Proof.Operations`    — state operations and size/bound theorems
    * `Proof.Connectivity`  — the `connected` predicate and preservation
    * `Proof.IsChain`       — `IsChain` preservation theorems for lists
    * `Proof.Realization`   — bridge from numeric `State` to concrete lists
    * `Proof.FindTrimIndex` — the `find_trim_index` walk and its analysis
-/

import Proof.Basic
import Proof.Operations
import Proof.Connectivity
import Proof.IsChain
import Proof.Realization
import Proof.FindTrimIndex

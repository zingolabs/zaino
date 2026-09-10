# zcashd support is removed; Zebra is the only backing validator

## Status

accepted

Supersedes [ADR-0001](0001-zcashd-support-feature-gate.md) and
[ADR-0005](0005-zcashd-support-default-off.md). Recorded after the fact:
the decision landed in zingolabs/zaino#1395 (commit 8c823f610, 2026-07-09),
which deleted the two records it reversed instead of superseding them. This
record restores the ledger's trail.

## Context and decision

ADR-0001 gated zcashd support behind the additive `zcashd_support` feature
and ADR-0005 made that feature opt-in, both as staging posts on a stated
deprecation path. zcashd itself was being retired upstream, the zcashd-shaped
launchers and live suites doubled the test matrix, and every zcashd-specific
parse path was a second implementation to keep correct.

The final step deletes the feature and everything it gated: the
zcashd-shaped `getpeerinfo` parsing, the zcashd validator launchers and
`ValidatorKind::Zcashd`, the zcashd-backed live suites, the zcashd stages of
the test-environment image, and the CI tasks that policed the gate. Zebra, or
another Zaino, is the only supported backing validator.

Zaino still serves the zcashd-compatible RPC surface to its own clients.
That wire contract, including field names, error codes, response shapes, and
the `zcashd_build` and `zcashd_subversion` protobuf fields, is Zaino's
product and does not depend on which validator backs the index.

## Consequences

- The `zcashd_support` feature no longer exists; a build that names it fails.
- The test-environment image builds only `final-prebuilt` and
  `final-zebrad-source`, and its tag no longer embeds a zcashd version.
- Comments that cite zcashd RPC semantics for the serving surface stay,
  because the surface they describe is still served.

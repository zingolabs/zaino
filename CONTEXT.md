# Zaino glossary

Canonical terms, where the team has picked one word among several.

## Release engineering

**Publishable set**:
The crates released to crates.io: the `[[target]]` entries in `relman.toml`,
which are every `packages/*` crate without `publish = false`.
_Avoid_: crate list, publish list

**Blocking context**:
A CI context in which release checks must pass: pushes to `stable`, whose tip is
the release commit.
_Avoid_: strict mode, release mode

**Advisory context**:
Any CI context that is not a blocking context. Release checks report findings
there but do not fail the build.
_Avoid_: soft mode, informational mode

**Version-reuse violation**:
A publishable crate whose exact version already exists on crates.io with
different packaged content. The tree cannot be released until that version is
bumped. An unchanged crate keeping its published version is not a violation.
_Avoid_: stale version, forgotten bump

## Chains and networks

**The Public Testnet**:
The public Zcash test network, and only that. Non-hermetic: state is shared
with other participants, and an epoch it has left (pre-NU6.3, since height
4,134,000) cannot be re-entered.
_Avoid_: bare "testnet" for any locally launched chain, even one launched under
a testnet network kind

**Regtest net**:
A hermetic, locally launched chain whose activation heights the launcher
chooses. Every hermetic local net is a regtest net, whatever network-kind flag
it runs under.
_Avoid_: local testnet, custom testnet

## Pools and upgrades

**Ironwood / Orchard (era naming)**:
Eras, fixtures and predicates about shielded pools are named by pool (Orchard,
Ironwood), and a name mentioning one pool pairs with the other pool's name,
never with the upgrade's. **NU6.3** names only the network upgrade: activation
heights, consensus branch ID, consensus rules.
_Avoid_: mixing vocabularies in one name or sibling set (an `ORCHARD_ONLY_*`
fixture's sibling is `IRONWOOD_ONLY_*`, not `NU6_3_ACTIVE_*`)

**Unfiltered pool set**:
The pools served when a request's `poolTypes` is empty: every shielded pool
(Sapling, Orchard, Ironwood), transparent excluded. Defined once, by
`zaino_index_compact_block::Pools::default`; a narrower set makes compact blocks disagree with
their own `chainMetadata` tree sizes, which a scanning wallet reads as a
phantom reorg.
_Avoid_: default pools, backfill set, "Sapling and Orchard"

**Cross-address restriction**:
The post-NU6.3 Orchard Action circuit rule that an output note carry the same
`(g_d, pk_d)` as the note it spends, so every Orchard action is change to the
spent note's own address or a withdrawal. Details:
[Ironwood activation](./docs/notes/ironwood-activation.md).
_Avoid_: "exit-only" (same-receiver change still lands in the pool, and its
commitment tree still grows)

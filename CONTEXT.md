# Zaino

Zaino is a Zcash indexing service. This glossary pins down the canonical
terms for concepts where the team has picked one word among several.

## Language

### Release engineering

**Publishable set**:
The workspace members released to crates.io as a unit — every member not
marked `publish = false`. Derived from workspace metadata, never from a
hard-coded list.
_Avoid_: crate list, publish list

**Blocking context**:
A CI context in which release checks must pass: pushes to `rc/**` or
`stable`, and pull requests targeting them.
_Avoid_: strict mode, release mode

**Advisory context**:
Any CI context that is not a blocking context. Release checks report
findings (warnings, annotations) but do not fail the build there.
_Avoid_: soft mode, informational mode

**Version-reuse violation**:
A publishable crate whose exact version already exists on crates.io while
its packaged content differs. The tree cannot be released until that crate's
version is bumped. An unchanged crate keeping its published version is not a
violation.
_Avoid_: stale version, forgotten bump

### Chains and networks

**Testnet**:
The public Zcash test network, and only that. Testnet regimes are
non-hermetic — state is shared with other participants, and an epoch
the public chain has left (e.g. pre-NU6.3 once NU6.3 activates there)
cannot be re-entered.
_Avoid_: "testnet" for any locally-launched chain, even one launched
under a testnet network kind

**Regtest net**:
A hermetic, locally-launched chain whose activation heights the
launcher chooses. Every hermetic local net is a regtest net, whatever
network-kind flag it runs under.
_Avoid_: local testnet, custom testnet

### Pools and upgrades

**Ironwood / Orchard (era naming)**:
Eras, fixtures, and predicates that speak of shielded pools are named by
pool — Orchard, Ironwood — and a name that mentions one pool pairs with
the other pool's name, never with the upgrade's. **NU6.3** names only the
network upgrade itself: activation heights, consensus branch ID,
consensus rules.
_Avoid_: mixing vocabularies in one name or one sibling set (e.g. an
`ORCHARD_ONLY_*` fixture whose sibling is `NU6_3_ACTIVE_*` — the sibling
is `IRONWOOD_ONLY_*`)

**Cross-address restriction**:
The post-NU6.3 rule the Orchard Action circuit enforces: "(g_d, pk_d)
of the output note must equal (g_d, pk_d) of the spent note" — the
output note must carry the same expanded receiver (diversified base
g_d, diversified transmission key pk_d) as its spent note, so each
Orchard action is either change to the spent note's own address or a
withdrawal (positive value balance). Orchard-to-Orchard transfers to
any other address — including another address of the same wallet — are
prohibited. A companion transaction-level rule forbids new value
entering the pool. Source:
<https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>
_Avoid_: "exit-only" (overclaims — same-receiver change still lands in
the pool and its commitment tree still grows)

### TLS and cryptography

**Preferred CryptoProvider**:
The TLS cryptography provider zaino installs as the process-wide default
when no provider is installed yet. A preference, not a mandate: an
embedder (e.g. zallet) that installs a provider before zaino keeps its
choice, and zaino handshakes through it.
_Avoid_: "enforced provider" (implies zaino overrides an embedder's
already-installed provider; it never does)

**Hybrid key exchange**:
A TLS 1.3 key-exchange group combining a classical curve with a
post-quantum KEM (key-encapsulation mechanism), e.g. X25519MLKEM768.
Secure if either component holds. Hybrids are the post-quantum
deployment vehicle, not a classical fallback.
_Avoid_: calling hybrids "classical" because they contain a classical
component

**Classical key exchange**:
A key-exchange group with no post-quantum component (X25519, SECP256R1,
SECP384R1). Deprecated in zaino: still accepted for client
compatibility, slated for refusal once major wallet stacks negotiate
hybrids.

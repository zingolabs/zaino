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

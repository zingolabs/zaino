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

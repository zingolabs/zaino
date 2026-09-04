# relman

Zaino's internal release manager: changeset-driven versioning, changelogs, and
release orchestration. Built as a hexagonal (ports & adapters) Rust CLI —
`relman-core` (types + ports), `relman-domain` (services), `relman-config`
(`relman.toml`), `relman-cli` (clap), and the `relman` binary (composition
root).

This is an **isolated Cargo workspace** (its own `[workspace]` root, like
`tools/workbench`): it is *not* a member of Zaino's production workspace and
every crate is `publish = false`. Build and test from within `tools/relman/`.

Design and intended crate topology: see
[`docs/release/implementation.md`](../../docs/release/implementation.md)
(section "`relman` internal structure (hexagonal)").

# Zaino Proto files module

This module encapsulates the lightclient-protocol functionality and imports the canonicals files
using `git subtree`.

<!-- TODO: this is a mess. fix later and vendor ourself?

Current state, for whoever picks this up:

- `lightwallet-protocol/` is a `git subtree` of https://github.com/zcash/lightwallet-protocol,
  currently at upstream tag **v0.5.0** (`git-subtree-split: ac7cee05`).
- It drifted badly before this: the subtree was recorded against the prefix
  `zaino-proto/lightwallet-protocol`, the crate then moved under `packages/`, and
  `git subtree pull` could no longer find its ancestry. Two years of upstream releases
  were hand-copied into the vendored files instead of pulled, including a partial
  re-implementation of upstream v0.5.0 filed under a hand-written changelog heading.
  Missing `ShieldedProtocol.ironwood` is what that cost us.
- We vendor rather than depend because Zaino is a *server*: `zcash_client_backend`
  generates only `compact_tx_streamer_client`, and the zcash org publishes protos, not
  Rust bindings. `zcash_client_backend` vendors this same directory for the same reason.
- zingolabs also publishes `lightwallet-protocol` (from `zingolabs/lightwallet_protocols`),
  which *does* generate the server. It is stale — no Ironwood, last released 2026-03-27 —
  so today it is not usable, and the org is maintaining two overlapping proto crates.

The open question is whether to own this properly (one zingolabs crate tracking upstream on
a real cadence, with `zaino-proto` reduced to `utils.rs` + `proposal.proto`), or to keep
vendoring and add a repo guard that fails when the subtree drifts from the tracked upstream
tag. Nothing here notices when upstream releases, which is the actual defect.

To pull upstream again:

    git subtree pull --prefix=packages/zaino-proto/lightwallet-protocol \
        https://github.com/zcash/lightwallet-protocol.git <tag> --squash

-->


Below you can see the structure of the module

````
zaino-proto
├── build.rs
├── build.rs.bak
├── Cargo.toml
├── CHANGELOG.md
├── lightwallet-protocol <=== this is the git subtree
│   ├── CHANGELOG.md
│   ├── LICENSE
│   └── walletrpc
│       ├── compact_formats.proto
│       └── service.proto
├── proto
│   ├── compact_formats.proto -> ../lightwallet-protocol/walletrpc/compact_formats.proto
│   ├── proposal.proto
│   └── service.proto -> ../lightwallet-protocol/walletrpc/service.proto
└── src
    ├── lib.rs
    ├── proto
    │   ├── compact_formats.rs
    │   ├── proposal.rs
    │   ├── service.rs
    │   └── utils.rs
    └── proto.rs
```

Handling maintaining the git subtree history has its own tricks. We recommend developers updating
zaino proto that they are wary of these shortcomings.

If you need to update the canonical files to for your feature, maintain a linear and simple git
commit history in your PR.

We recommend that PRs that change the reference to the git subtree do so in this fashion.

for example:
============

when doing
```
git subtree --prefix=zaino-proto/lightwallet-protocol pull git@github.com:zcash/lightwallet-protocol.git v0.5.0 --squash
```

your branch's commits must be sequenced like this.

```
  your-branch-name
    - commit applying the git subtree command
    - commit merging the canonical files
    - commits fixing compiler errors
    - commit indicating the version adopted in the CHANGELOG.md of zaino-proto
```

If you are developing the `lightclient-protocol` and adopting it on Zaino, it is recommended that
you don't do subsequent `git subtree` to revisions and always rebase against the latest latest version
that you will be using in your latest commit to avoid rebasing issues and also keeping a coherent
git commit history for when your branch merges to `dev`.

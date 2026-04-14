# Zaino Proto files module

This module encapsulates the lightclient-protocol functionality and imports the canonicals files
using `git subtree`.


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
git subtree --prefix=zaino-proto/lightwallet-protocol pull git@github.com:zcash/lightwallet-protocol.git v0.4.0 --squash
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


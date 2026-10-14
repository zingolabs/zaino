---
default: minor
---

# `zaino-persistence`: `manifest_fits(schema, fanout)`

`manifest_fits` says whether a store of `schema` under `fanout` keeps its manifest within its slot at the worst case (every map at the stall bound in every tier); callers check it before opening rather than panic on a commit.

# No upstream types in public APIs

## Status

accepted

## Context and decision

There are several advantages to exposing upstream types in a public API, the largest one being an ability to expose the functionality of the upstream types directly to the consumer. Becuase of this, many of our libraries currently expose upstream types in their public APIs. The downsides to this approach were less initially apparent.

These advantages are outweighed by the increased cost of managing dependency versions, as having upstream types in a public API creates a requirement that the version of the upstream crate must match between the library which exposes the upstream types and the crate which depends on that library. For example, if infrastructure exposes zcash_primitives types, then a crate which depends on infrastructure must depend on the same version of zcash_primitives that infrastructure does. 

This is a simple requirement in theory, but when enough dependencies get involved they can all get tangled together, as is the case with zaino's update to using zebra 3.0.0. The complexity of dependency management during this update has been too extreme. 
The solution, which zingolabs members arrived at consensus at, is to stop exposing upstream types in public APIs. No further upstream types should be allowed into pub interfaces, and work will be done to incrementally remove upstream types from public APIs that already contain them, with the goal of removing enough by the time that we next must update our zebra dependency that the required work will be much simpler.

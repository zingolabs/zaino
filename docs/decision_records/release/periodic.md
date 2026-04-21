# This Decision Record Communicates our strategy for periodic releases

### Periodic Releases

Why?

* predictability for remote partners
* refereancable version for all consumers
* stable cadence for internal planning

##### Periodic Releases

  Developers innovate on the shared mind-state by diverging along their own mental trajectory tracking the evolution as a  "feature branch".   Once a feature branch is approved by collaborators on a project it can be merged into "dev".

  Merges into dev should be frequent and coherent in order to maximize shared understanding.

  Every 14 days, April 24, April 38, April 52, ....  a new periodic release merges from "release" in to "stable".


In order to be considered for release, code must have landed on dev before the 1 week prior "relese candidate genesis" moment.

After the genesis dev and release are not constrained to be the same and may diverge.  "dev" maintains its identity as new shared understanding of the code, while "release" accumulates minimal tagged changes (called "candidates") that are proposed to be important for public consumers, stable, and integrated with other (on release features).

Any update to "release" must pass the following set of "gates" that assure it has the integrated, stable, and useful properties listed above.

Gates:

   * all integration tests pass
   * all unit tests pass
   * the candidate has run..  "in the wild" for 48 hours

This means that no release candidate will make it into the "current" periodic release if it is not on "release" at least 48 hours prior to the "periodic" release moment.

##### Periodic Release Protocol

 (1) set the version numbers of changed relative to dev packages to the correct semver numbers
      * this is new commit on release, the commit is uniquely ahead of the rc tag
      * the versioned commit is tagged "periodic-DATE-A.x.y.z-B.x.y.z-C...."
      * the versioned commit will have all changelogs updated such that any changes previously listed as unreleased are now listed in the new release number, and the unreleased sections are empty.
 (2) cargo-publish all changed crates
 (3) publish new zainod to container repository
 (4) merge release commit into stable
 (5) merge new stable into dev

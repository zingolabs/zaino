# This Decision Record Communicates our release strategy

### Periodic Releases

* predictability for remote partners
* refereancable version for all consumers
* stable cadence for internal planning

##### Process

  flowchart TD
      FB["feature branch<br/>(new code)"]
      DEV["dev branch<br/>(merges frequent & coherent)"]
      INT["integration-and-stress tests<br/>on latest dev commit"]
      RC["release branch<br/>commit tagged <b>rc</b>"]
      REL_TEST["release-tests<br/>against the rc"]
      NASCENT["nascent release =<br/>most recent rc that passed release-tests"]
      VER["set semver on changed crates<br/>commit tagged
  <b>periodic-DATE-A.x.y.z-B.x.y.z-…</b><br/>update CHANGELOG: Unreleased → this
  release"]
      STABLE["merge release → <b>stable</b><br/>(every 14 days: Apr 24, May 8, May 22,
  …)"]
      PUB["cargo publish changed crates<br/>(from stable)"]
      CONT["publish zainod container image<br/>(from stable)"]
      BACK["merge stable → dev"]

      FB -->|"collaborator approval"| DEV
      DEV --> INT
      INT -->|"fail: wait for next dev commit"| DEV
      INT -->|"pass"| RC
      RC --> REL_TEST
      REL_TEST -->|"fail: skip this rc"| RC
      REL_TEST -->|"pass"| NASCENT
      NASCENT -->|"on release date"| VER
      VER --> STABLE
      STABLE --> PUB
      PUB --> CONT
      CONT --> BACK
      BACK -.->|"cycle continues"| DEV

  Developers innovate on the shared mind-state by diverging along their own mental trajectory tracking the evolution as a  "feature branch".   Once a feature branch is approved by collaborators on a project it can be merged into "dev".

  Merges into dev should be frequent and coherent in order to maximize shared understanding.

  Every 14 days, April 24, April 38, April 52, ....  a new periodic release merges from "release" in to "stable".

  Periodically full-integration-and-stress tests run on the latest dev commit.

  Any dev commit that passes all integtration-and-stress tests is merged to the release branch with an "rc" tag, and a release-tests flow is triggered against the release candidate.

  The most recent release candidate that has passed the "release-tests" is the nascent release.  The nascent release is merged into stable, and tagged on the release date.

##### Periodic Release Protocol

 (1) set the version numbers of changed relative to dev packages to the correct semver numbers
      * this is new commit on release, the commit is uniquely ahead of the rc tag
      * the versioned commit is tagged "periodic-DATE-A.x.y.z-B.x.y.z-C...."
      * the versioned commit will have all changelogs updated such that any changes previously listed as unreleased are now listed in the new release number, and the unreleased sections are empty.
 (2) merge release commit into stable
 (3) cargo-publish all changed crates from stable
 (4) publish new zainod to container repository from stable
 (5) merge new stable into dev

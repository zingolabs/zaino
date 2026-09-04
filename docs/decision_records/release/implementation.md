# Release Pipeline — Implementation Architecture

> Sub-spec of [pipeline.md](./pipeline.md). Where `pipeline.md` states the
> *what* and *why* (branches, gates, identity, hotfix), this document states the
> *how*: which execution substrate runs each piece, the boundary between the
> `relman` CLI and the CI glue, and the GitHub↔cluster bridge for the deployment gate.
> No code specifics — responsibilities and boundaries only.

## Execution substrates

The pipeline is **not** "just CI." It spans two execution substrates plus a
bridge, because the `release`-gate (deployment) is a fundamentally different beast
from the in-repo gates — days-long live-chain deployments, not nextest runs.

| Piece | Substrate | Notes |
| ----- | --------- | ----- |
| `dev`-gate (unit + integration + e2e smoke), changeset `check` + bot rename | **GitHub Actions** (nextest) | pre-merge, per-push, in-repo |
| `rc`-gate (full e2e), advance `rc` | **GitHub Actions** (nextest, nightly) | in-repo, scheduled |
| changeset aggregation, version derivation, bump, changelog, release-PR body, tag planning, publish planning | **GitHub Actions + `relman`** | control-plane logic |
| branch protection, sentinels, CODEOWNERS | **GitHub** (rulesets) | platform-native |
| **`release`-gate (deployment / days-long live-chain runs)** | **the cluster — Argo** | ArgoCD + Argo Workflows + Argo Events; live chains; *not* nextest, *not* GH runners |

**GitHub Actions is the control plane** (branches, versions, PRs, publish,
in-repo gates); **the cluster is the deployment data plane**. In-repo tests are
nextest-gated; the deployment gate is Argo-driven and evaluated against metrics, not a test
runner.

## `relman`: functional core, imperative shell

`relman` (a new sibling crate under `tools/`, see [pipeline.md §
Implementation](./pipeline.md#implementation)) is the **functional core**: it
makes every deterministic decision and performs every *working-tree* edit, with
**no network and no ref/remote mutation**, so it is unit-testable and safe to
run locally (dry by default). The CI YAML is the **imperative shell**: it only
applies `relman`'s outputs as side-effects on the outside world.

### What `relman` owns

| Subcommand (area) | Reads | Produces | Touches |
| ----------------- | ----- | -------- | ------- |
| `changeset new [--empty <reason>]` | — | a `.changesets/<slug>.toml` scaffold | working tree |
| `changeset check` | git diff vs base, `.changesets/`, crate graph | pass/fail + diagnostics (enforcement) | nothing (read-only) |
| `changeset rename --pr <N>` | `.changesets/` | renamed `pr-<N>.toml` | working tree |
| `derive` | `.changesets/`, all `Cargo.toml`, crate graph | per-crate next-version table (highest-`kind` + pre-1.0 map + transitive) | nothing (read-only) |
| `bump` | derive output | edited `Cargo.toml` versions + root `[workspace.dependencies]` pins (via `toml_edit`, format-preserving) | working tree |
| `changelog` | `.changesets/`, derive output | per-crate + workspace changelog edits | working tree |
| `pr-body` | derive output, deployment status input | rendered release-PR description | stdout/file |
| `tags` | derive output, cycle id | the tag set to apply (`<crate>-vX.Y.Z`, `cycle-<id>`, `cycle-<id>-rc.N`) | stdout/file |
| `publish-plan` | crate graph, crates.io state (reuse `workbench check-published-versions`) | topo-ordered publish list, skipping unchanged | stdout/file |
| `changeset clear` (release consume) | `.changesets/` | empties `.changesets/` | working tree |

### What CI glue owns (and nothing more)

The workflows **decide nothing**; they loop over `relman`'s emitted plans and
do the dumb execution:

- commit the files `bump` / `changelog` / `rename` / `clear` wrote, and push;
- `git tag` each entry from `tags`, and push;
- `gh pr edit` with the `pr-body` output; open/advance the sentinel PRs;
- `cargo publish` in `publish-plan` order;
- build/push Docker images; create the GitHub Release;
- create GitHub **Deployments** and react to `deployment_status` (the
  deployment-gate bridge, below).

All *judgment* — which version, which tags, which changelog lines, which publish
order, whether a PR satisfies changeset enforcement — is in Rust, tested, and
runnable by a maintainer by hand. Bash never re-derives anything.

### Not knope (or any external release manager)

`knope` / `release-plz` / `cargo-release` are opinionated and expect you inside
*their* flow. Our model — version-agnostic branches, cycle tags, continuous
per-commit deployment, derived-only versions, PR-numbered changesets, semantic `kind`
— diverges enough that config-bending costs more than a small purpose-built
tool. `relman` is a handful of pure functions plus `toml_edit`. We borrow the
*idea* (changeset-driven derivation), not the tool.

### `relman` internal structure (hexagonal)

`relman` is generated from the `rust-cli-starter` `cargo-generate` template
(ports & adapters), as its **own isolated workspace** under `tools/relman/`
(kept out of the production graph, like `workbench`). The template's
functional-core/imperative-shell shape *is* the boundary specified above.

| Crate | Role |
| ----- | ---- |
| `relman-core` | pure types (newtype-per-module, parse-don't-validate) + port traits |
| `relman-domain` | the derivation services (aggregation, highest-`kind` + pre-1.0 map, transitive bumps) — unit-tested against mocks. The heart. |
| `relman-config` | parses the committed `relman.toml` into a typed `ReleaseConfig` (targets + options) at the boundary |
| `relman-adapters` | concrete driven ports: fs changeset/changelog store, `toml_edit`/`cargo_metadata` workspace + manifest editor, git subprocess, crates.io index (reuse `workbench`) |
| `relman-cli` | clap delivery adapter — subcommands call driving ports via a `Ctx` |
| `relman` (bin) | composition root; the only place naming concrete adapters |

- **Driven ports** (what relman needs): `Workspace`, `ChangesetStore`,
  `ManifestEditor`, `ChangelogStore`, `Vcs`, `Registry`. **Driving ports** (what
  relman offers, one per CLI concern): `Changesets`, `Versions`, `Bump`,
  `Changelog`, `ReleaseArtifacts`.

#### `relman.toml` — the versioning-target manifest

A **repo-committed** manifest (not an XDG user config) is the single source of
truth for what relman governs. It lists each versioning target, its location,
and per-target options; `relman-config` parses it into typed core newtypes at
the composition root.

```toml
# relman.toml — repo root
[options]
changesets_dir      = ".changesets"
root_manifest       = "Cargo.toml"        # where [workspace.dependencies] pins live
workspace_changelog = "CHANGELOG.md"

[[target]]
name = "zaino-state"
path = "packages/zaino-state"
# changelog defaults to <path>/CHANGELOG.md; publish defaults to true

[[target]]
name = "zainod"
path = "packages/zainod"
```

This is **the** authority for the governed-target set: changeset enforcement
validates a change's `crate` against the declared targets, versions/bumps are
applied only to declared targets, and the publish plan covers exactly them. No
`cargo metadata` heuristic decides governance.

- **Pruned from the template:** the `installer` crate (relman runs in-repo, no
  XDG install) and the `status`/`Health` demo thread.
- **Kept & repurposed:** the `config` crate (parses `relman.toml`, above); the
  `Clock` driven port (relman needs "today" to date
  `## [x.y.z] - YYYY-MM-DD` changelog headers); the `mocks`/`test-support` seam
  (what makes the derivation services testable).
- **Conventions inherited:** no `mod.rs`, parse-don't-validate newtypes, depend
  on `dyn Trait` (concretes only in the binary).

## The deployment-gate bridge: GitHub Deployments ↔ Argo

> **Status.** The **GitHub side of this bridge is built**: `rc-gate` creates the
> Deployment on a cut (step 1) and `deployment-advance.yml` reacts to
> `deployment_status` by fast-forwarding `release-ready` (step 6). The **cluster
> side** (Argo Events eventsource + sensor + the soak Workflow, steps 2–5) lives
> in the `devops` repo and is the remaining build. `tools/scripts/mark-deployment.sh`
> injects the `deployment_status` so the GitHub half is testable without the
> cluster.

Each RC commit is dispatched to the **deployment gate** via a **GitHub
Deployment** targeting a dedicated `deployment` environment, executed by the
cluster. Routing it through GitHub's Deployments primitive makes each
deployment run native and visible (it shows on the commit and PR, gets
environment protection rules) — the "visible marker" story for the
`release`-gate.

**GitHub → cluster (start a deployment run):**
1. On `rc`-gate pass, a GH Action creates a **GitHub Deployment** for the commit
   + its `cycle-<id>-rc.N` image.
2. An **Argo Events** webhook eventsource + sensor catches the GitHub Deployment
   event and fires the deployment **Argo Workflow**.
3. The 3–4 deployment slots + queue are an Argo Workflow **semaphore**; the
   [coalesce-to-latest](./pipeline.md#the-release-gate-continuous-deployment) rule
   lives in the sensor (drop a queued commit already superseded by a newer
   `gate/candidate`).

**cluster → GitHub (report result):**
4. The deployment Workflow boots mainnet **`zebra`** + the RC's **`zainod`** and
   exercises it. Depth is a **dial, not a fixed days-long soak**:
   - **warm-start** from the golden synced snapshot (`serve-zaino use-cache=true`)
     and validate at the **tip** in minutes–hours — wallet-sync fixtures, sending
     transactions, light-RPC probes; or
   - **fresh full-index sync** from genesis (hours–days) when a release warrants it.
   Validation is **automated** (fixtures + metrics: sync completed, no crash, perf
   in bounds) **and/or manual** — a tester syncs a wallet against it, sends txs,
   and signs off (an Argo `suspend` step or a GitHub environment approval).
5. Its final step reports **`deployment_status`** back to GitHub
   (`in_progress` → `success` / `failure`). The poster is deliberately
   unopinionated: the automated Workflow, a fixture job, or a **human** (via the
   environment approval or `mark-deployment.sh`) — the GitHub side reacts the same.
6. `deployment-advance.yml` reacts to `deployment_status`: on `success`,
   fast-forwards `release-ready` (which refreshes the release PR); on `failure`,
   the frontier stays put and the team fixes forward on `dev`.

One platform primitive (Deployments) carries the bridge in both directions —
preferred over a raw `repository_dispatch` precisely for the native visibility
and protection rules.

### Testability: never wait days to test the pipeline

The deployment gate is days-long in production, but *duration must never be the
thing under test*. Two properties are **built in** so the whole pipeline is
testable in seconds/minutes:

1. **Duration is a parameter, not a constant.** The deployment Argo
   `WorkflowTemplate` takes a `duration` (and sync-target/chain) input.
   Production = "days on mainnet"; a test run = "~10 minutes on regtest/a short
   chain", evaluating the *same* pass/fail criteria over a shorter window. The
   gate's logic (deploy → observe → evaluate metrics → verdict) is identical;
   only the window shrinks. So the gate's behaviour is validated without the
   endurance wait — and the genuine endurance property is a separate, infra-side
   concern with its own short-window checks.

2. **The pass/fail signal is injectable.** Because the bridge is a GitHub
   Deployment → `deployment_status` callback, the *pipeline's reaction* is
   testable completely independently of any real deployment run: create a GitHub
   Deployment for an RC commit and POST `deployment_status = success` (or
   `failure`) by hand. That fires the `release-ready` advance (step 6) — proving
   the `rc → release-ready` promotion + the release-PR refresh + blessing **with
   no deployment run at all**. The helper `tools/scripts/mark-deployment.sh`
   wraps this one `gh api` call.

Corollary for the reacting GH Action (step 6): it must key off the real
`deployment_status` event/payload only — no hidden assumption that a run
actually happened — so a simulated status is indistinguishable from a real one.
This keeps the mechanics test path (dispatch + simulated status) and the
endurance test path (short-duration real run) fully decoupled.

## Ownership split (two repos)

The bridge is the only contract between the repos, so responsibilities divide
cleanly:

- **`zaino` repo:** `relman`, the GitHub Actions workflows, branch/PR policy
  (rulesets, CODEOWNERS), and Deployment *creation* + `deployment_status`
  *reaction*.
- **`devops` repo:** the deployment Argo `WorkflowTemplate`, the Argo Events
  eventsource/sensor, the `deployment` environment, and the metrics-threshold
  evaluation.

The contract is a **Deployment event schema** (commit sha, image ref, cycle id)
and the `deployment_status` callback. Either side can evolve independently as
long as that schema holds.

## Trust model

One place for "what credentials exist, who can reach them, and what can cause a
release". The workflows carry per-step comments; this section is the inventory.

### Credentials

| Secret | What it is | Where it is used |
| --- | --- | --- |
| `RELEASE_APP_ID` / `RELEASE_APP_PRIVATE_KEY` | GitHub App. Its pushes bypass protected-branch rules and trigger downstream workflows (a `GITHUB_TOKEN` push does neither). | `rc-gate`, `blessing`, `deployment-advance`, `changeset-rename`, `backport-sentinel`, `release-pr-body` |
| `CARGO_REGISTRY_TOKEN` | Stored long-lived crates.io publish token for every publishable crate. | `blessing` only (pre-flight check + publish) |
| `DISPATCH_APP_ID` / `DISPATCH_APP_PRIVATE_KEY` | Pre-existing App for cross-repo test dispatch. Not part of this pipeline. | `trigger-integration-tests` |

Open item: replace the stored `CARGO_REGISTRY_TOKEN` with crates.io Trusted
Publishing (per-run OIDC federation, no stored credential) — under evaluation
in the PR discussion. If the stored token stays, the reason gets recorded here.

### Exposure

Workflows that mint an App token on `pull_request` (`changeset-rename`) run
the PR branch's copy of the workflow file with secrets available. A same-repo
PR can therefore modify the workflow and reach the App key. This is the
trusted-writer posture: write access to this repo means release-credential
access. Fork PRs receive no secrets. `changeset-check` is the one ungated
workflow and holds no credentials — it only reads the diff.

### What can cause a release to advance

Every path below is inert until the repo variable `RELMAN_PIPELINE_ACTIVE`
is `true`; `RELMAN_PUBLISH_DRY_RUN` additionally downgrades publishing to
advisory.

1. **Merge/push to `stable`** → `blessing` publishes to crates.io, tags, cuts
   the GitHub Release. `stable` is protected; the release App bypasses the
   protection by design.
2. **`rc-gate` nightly cron** advances the rc frontier when the gate condition
   holds. Its `workflow_dispatch` input `force=true` bypasses the gate and is
   available to any write-access user; until the nightly green precondition is
   wired, `force` is the only advance path (flagged in the workflow header as
   the draft escape hatch — do not activate the pipeline before closing it).
3. **Any `deployment_status: success` event** on a gated Deployment →
   `deployment-advance` moves `release-ready`. The event's authenticity is not
   verified beyond repo write access to post it.

## Dependency: deployment pass/fail needs metrics

"Did the deployment gate pass" is a metrics question — sync completed, no crash, performance
within bounds — so the Argo Workflow queries **Prometheus** for its verdict.
This ties the `release`-gate to the observability work (`feature/prometheus-metrics`).
Until those metrics and thresholds exist, the deployment gate can run in an **advisory
/ manual-attestation** mode: the deployment happens and is observed, but the
`release-ready` advance is a human call recorded on the dashboard, not an
automated `deployment_status` gate.

## Buildability & sequencing

The **changeset → version → changelog → PR → publish machinery is "GitHub
Actions + `relman`"** — no cluster required. That is the bulk of the work and is
buildable now (slices 1–4 in the build plan). The **deployment gate is the
infra-heavy, separable tail**: it needs the Argo Workflow + Events + Deployments
bridge + metrics criteria, lives mostly in `devops`, and the rest of the
pipeline functions without it — the deployment gate simply stays manual attestation until the
bridge lands.

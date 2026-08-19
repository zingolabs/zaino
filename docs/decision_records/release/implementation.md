# Release Pipeline — Implementation Architecture

> Sub-spec of [pipeline.md](./pipeline.md). Where `pipeline.md` states the
> *what* and *why* (branches, gates, identity, hotfix), this document states the
> *how*: which execution substrate runs each piece, the boundary between the
> `relman` CLI and the CI glue, and the GitHub↔cluster bridge for the soak gate.
> No code specifics — responsibilities and boundaries only.

## Execution substrates

The pipeline is **not** "just CI." It spans two execution substrates plus a
bridge, because the `release`-gate (soak) is a fundamentally different beast
from the in-repo gates — days-long live-chain deployments, not nextest runs.

| Piece | Substrate | Notes |
| ----- | --------- | ----- |
| `dev`-gate (unit + integration + e2e smoke), changeset `check` + bot rename | **GitHub Actions** (nextest) | pre-merge, per-push, in-repo |
| `rc`-gate (full e2e), advance `rc` | **GitHub Actions** (nextest, nightly) | in-repo, scheduled |
| changeset aggregation, version derivation, bump, changelog, release-PR body, tag planning, publish planning | **GitHub Actions + `relman`** | control-plane logic |
| branch protection, sentinels, CODEOWNERS | **GitHub** (rulesets) | platform-native |
| **`release`-gate (soak / long deployments)** | **the cluster — Argo** | ArgoCD + Argo Workflows + Argo Events; live chains; *not* nextest, *not* GH runners |

**GitHub Actions is the control plane** (branches, versions, PRs, publish,
in-repo gates); **the cluster is the soak data plane**. In-repo tests are
nextest-gated; soak is Argo-driven and evaluated against metrics, not a test
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
| `pr-body` | derive output, soak status input | rendered release-PR description | stdout/file |
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
- create GitHub **Deployments** and react to `deployment_status` (the soak
  bridge, below).

All *judgment* — which version, which tags, which changelog lines, which publish
order, whether a PR satisfies changeset enforcement — is in Rust, tested, and
runnable by a maintainer by hand. Bash never re-derives anything.

### Not knope (or any external release manager)

`knope` / `release-plz` / `cargo-release` are opinionated and expect you inside
*their* flow. Our model — version-agnostic branches, cycle tags, continuous
per-commit soak, derived-only versions, PR-numbered changesets, semantic `kind`
— diverges enough that config-bending costs more than a small purpose-built
tool. `relman` is a handful of pure functions plus `toml_edit`. We borrow the
*idea* (changeset-driven derivation), not the tool.

## The soak bridge: GitHub Deployments ↔ Argo

The `release`-gate is modeled as a **GitHub Deployment to a `soak`
environment**, executed by the cluster. This makes a soak run native and
visible (it shows on the commit and PR, gets environment protection rules) — the
"visible marker" story for the `release`-gate.

**GitHub → cluster (start a soak):**
1. On `rc`-gate pass, a GH Action creates a Deployment for the commit + its
   `cycle-<id>-rc.N` image.
2. An **Argo Events** webhook eventsource + sensor catches the Deployment event
   and fires a soak **Argo Workflow**.
3. The 3–4 soak slots + queue are an Argo Workflow **semaphore**; the
   [coalesce-to-latest](./pipeline.md#the-release-gate-continuous-soak) rule
   lives in the sensor (drop a queued commit already superseded by a newer
   `gate/candidate`).

**cluster → GitHub (report result):**
4. The soak Workflow drives a full-chain sync on live data, then evaluates
   pass/fail against **metrics thresholds** (sync completed, no crash, perf in
   bounds).
5. Its final step reports **`deployment_status`** back to GitHub
   (`in_progress` → `success` / `failure`).
6. A GH Action reacts to `deployment_status`: on `success`, advance
   `release-ready` and refresh the release-PR body; on `failure`, record it on
   the dashboard and leave the frontier where it is (fix-forward on `dev`).

One platform primitive (Deployments) carries the bridge in both directions —
preferred over a raw `repository_dispatch` precisely for the native visibility
and protection rules.

## Ownership split (two repos)

The bridge is the only contract between the repos, so responsibilities divide
cleanly:

- **`zaino` repo:** `relman`, the GitHub Actions workflows, branch/PR policy
  (rulesets, CODEOWNERS), and Deployment *creation* + `deployment_status`
  *reaction*.
- **`devops` repo:** the soak Argo `WorkflowTemplate`, the Argo Events
  eventsource/sensor, the `soak` environment, and the metrics-threshold
  evaluation.

The contract is a **Deployment event schema** (commit sha, image ref, cycle id)
and the `deployment_status` callback. Either side can evolve independently as
long as that schema holds.

## Dependency: soak pass/fail needs metrics

"Did soak pass" is a metrics question — sync completed, no crash, performance
within bounds — so the Argo Workflow queries **Prometheus** for its verdict.
This ties the `release`-gate to the observability work (`feature/prometheus-metrics`).
Until those metrics and thresholds exist, the soak gate can run in an **advisory
/ manual-attestation** mode: the deployment happens and is observed, but the
`release-ready` advance is a human call recorded on the dashboard, not an
automated `deployment_status` gate.

## Buildability & sequencing

The **changeset → version → changelog → PR → publish machinery is "GitHub
Actions + `relman`"** — no cluster required. That is the bulk of the work and is
buildable now (slices 1–4 in the build plan). The **soak gate is the
infra-heavy, separable tail**: it needs the Argo Workflow + Events + Deployments
bridge + metrics criteria, lives mostly in `devops`, and the rest of the
pipeline functions without it — soak simply stays manual attestation until the
bridge lands.

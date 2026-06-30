# Issue tracker: GitHub (via `gh` CLI)

Issues and PRDs for this repo live on **GitHub**, in the **`zingolabs/zaino`**
repository. All operations go through the **`gh` CLI** (and `gh api` for anything
the porcelain doesn't cover) — not Linear, not an MCP server. `gh` is already
authenticated in this environment; no schema-loading step is needed.

GitHub **is** the triage surface here. Unlike repos that mirror issues into an
external tracker, both issues and pull requests on `zingolabs/zaino` are
first-class triage inputs; `/triage` reads from GitHub directly.

## Conventions

Always scope to `--repo zingolabs/zaino` (the local checkout has several forks as
remotes — `nuttycom`, `pacu`, `valar`, `zecrocks` — so never rely on the implicit
default).

- **Create an issue**: `gh issue create --repo zingolabs/zaino --title "…"
  --body "…"`. Pass `--label`/`--assignee` per `triage-labels.md`. The body is
  markdown — use real newlines, not `\n`.
- **Read an issue**: `gh issue view <number> --repo zingolabs/zaino --comments`
  to pull the issue plus its discussion thread.
- **List issues**: `gh issue list --repo zingolabs/zaino` with filters
  (`--state`, `--label`, `--search`, `--assignee`, `--milestone`). For richer
  queries use `gh search issues --repo zingolabs/zaino …`.
- **Comment on an issue**: `gh issue comment <number> --repo zingolabs/zaino
  --body "…"`.
- **Apply labels / change state**: `gh issue edit <number> --repo zingolabs/zaino
  --add-label … --remove-label …`; open/close with `gh issue close`/`gh issue
  reopen`. Create a missing label first with `gh label create`.
- **List labels**: `gh label list --repo zingolabs/zaino`.

### Repo facts (snapshot — re-list to confirm)

- **State model**: GitHub issues are **open** or **closed** — there is no
  multi-stage status column. Workflow stages that a status would carry elsewhere
  are expressed as **labels** (see `triage-labels.md`).
- **Labels** (partial — run `gh label list` for the live set): `bug`,
  `enhancement`, `documentation`, `refactor`, `tech-debt`, `RELEASE BLOCKER`,
  `do not merge`, `Top Priority` / `Low Priority`, `Zallet`,
  `Framework Adaptation`, the `ZGM1`/`ZGM2`/`ZGM3` milestone labels, and the
  `zainod` / `zainolib` component labels. Triage-role labels are created lazily —
  see `triage-labels.md`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue in `zingolabs/zaino` with `gh issue create`.

## When a skill says "fetch the relevant ticket"

Resolve the GitHub issue with `gh issue view <number> --comments` (add
`--repo zingolabs/zaino`).

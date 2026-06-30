# Triage Labels

The skills speak in terms of five canonical triage roles. This repo tracks issues
on **GitHub** (`zingolabs/zaino`), which has no multi-stage status column — only
**open**/**closed** plus **labels**. So every role is expressed as a GitHub
**label** (and `wontfix` additionally **closes** the issue). See
`issue-tracker.md` for the `gh` tooling.

| Role in mattpocock/skills | How we express it on GitHub                  | Meaning                                  |
| ------------------------- | -------------------------------------------- | ---------------------------------------- |
| `needs-triage`            | open + label **`needs-triage`**              | Maintainer needs to evaluate this issue  |
| `needs-info`              | label **`needs-info`**                       | Waiting on reporter for more information |
| `ready-for-agent`         | open + label **`ready-for-agent`**           | Fully specified, ready for an AFK agent  |
| `ready-for-human`         | open + label **`ready-for-human`**           | Requires human implementation            |
| `wontfix`                 | **closed** + label **`wontfix`**             | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), translate
it via this table: apply the GitHub **label** shown with
`gh issue edit <number> --repo zingolabs/zaino --add-label <label>`, and for
`wontfix` also `gh issue close <number>`.

Notes:

- **None of these five labels exist in `zingolabs/zaino` yet** — create each on
  first use with
  `gh label create <label> --repo zingolabs/zaino --description "…"`, then apply
  via `gh issue edit --add-label`.
- Because GitHub has no "Todo"-style status, `ready-for-agent` and
  `ready-for-human` are distinguished **only by their labels** — there is no
  implicit "triaged and ready" state to share. An open issue carrying neither a
  `ready-for-*` label nor `needs-info` is still `needs-triage`.
- These triage-role labels are orthogonal to the repo's existing topical labels
  (`bug`, `enhancement`, `refactor`, `Top Priority`, the `ZGM*` milestones, the
  `zainod`/`zainolib` component labels, …); an issue normally carries one triage
  label plus any number of topical ones.

Edit this table if the workflow changes.

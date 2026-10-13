---
default: major
---

# `zainod`: `/statusz` and `/readyz` report one closed `state`: `provisioning`, `syncing`, `serving` or `down` (first …

`/statusz` and `/readyz` report one closed `state`: `provisioning`, `syncing`, `serving` or `down` (first match). `/statusz` adds `progress` (`{done, total, unit, rate}`, blocks or bootstrap bytes, only while provisioning or syncing), `detail` (why, only when down) and `notes` (one sentence per raised alarm); `ready` = `state` is `serving`. `/readyz` answers `{"state", "detail"}`, `200` only when serving. A tip held only by validators still catching up is `syncing`, never `serving`; boot waiting on its first validator answer is `down`. `reasons` (both endpoints) and `unready` (`/statusz`) are gone.

**Migration:** Read `state` (and `detail` when `down`) instead of `reasons` / `unready`.

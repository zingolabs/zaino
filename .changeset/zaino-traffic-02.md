---
default: minor
---

# `zaino-traffic`: `Observation.health` = the member's health right after that poll, `MemberRow.failures` = its consecutive …

`Observation.health` = the member's health right after that poll, `MemberRow.failures` = its consecutive failures, `Health::label()` = its metric label, and `Unanswered` implements `Display` (its last failure or answer, or "no member to ask").

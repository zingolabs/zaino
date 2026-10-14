---
default: major
---

# `zaino-traffic`: one trusted-validator health, carried on each `Observation`

`MemberRow::health` is removed: a trusted validator's health is the one its poll `Observation` carries, and the chain view stores it as-is (`/status` and the gauges read the same value). The event-order stamps behind the T3/T6 history checks are gone; the model's oracle checks both per send.

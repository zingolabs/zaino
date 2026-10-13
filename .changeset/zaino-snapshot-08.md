---
default: major
---

# `zaino-snapshot`: gauge `zaino_chainview_best_height` removed; `zaino_best_tip` carries the same height

Gauge `zaino_chainview_best_height` removed: it always equalled `zaino_best_tip` (the header chain's verified best, G3), which stays.

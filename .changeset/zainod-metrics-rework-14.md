---
default: patch
---

# `zainod`: A `metrics_endpoint` that fails to bind now fails startup with the error, instead of leaving the process …

A `metrics_endpoint` that fails to bind now fails startup with the error, instead of leaving the process running with no `/metrics`, no `/livez`, and no upkeep of the recorder's histogram samples. The listener is bound before the recorder is installed.

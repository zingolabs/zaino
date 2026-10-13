---
default: major
---

# `zainod`: Failure policy: any panic aborts the process, and the first long-running task to end stops zainod (no …

Failure policy: any panic aborts the process, and the first long-running task to end stops zainod (no in-process restart). A failed index commit panics naming the index and its directory.

**Migration:** Run zainod under a service manager that restarts it.

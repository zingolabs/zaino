---
default: major
---

# `zaino-source`: `BlockchainInfo::schedule` refuses a schedule without Sapling

`BlockchainInfo::schedule()` returns `Result<LocalNetwork, ScheduleError>`: `ScheduleError::OutOfOrder` for upgrades listed out of protocol order (was `None`) and the new `ScheduleError::NoSapling` for a listing without Sapling, which no served pool can start from. `Timeouts::DEFAULT` is gone (use `Default`).

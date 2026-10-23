---
default: minor
---

# `zainod verify` checks every index file against its page checksums

`zainod verify [-c FILE]` reads every file each enabled index has made durable and checks every
page's CRC. It is read-only and safe beside a running zainod: it opens nothing for writing,
creates no directory and takes no lock. Bytes a live writer appended past the durable length are
reported as orphaned, not as corruption.

Prints a JSON report on stdout and a summary on stderr. Exits 0 when every page is intact, 1 when
any file is lost or corrupt, 2 when the indexes could not be read.

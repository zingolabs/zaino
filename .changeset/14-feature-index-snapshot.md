---
default: minor
---

# `zainod`: `[snapshot]` fills empty indexes from a published IndexSnapshot

```toml
[snapshot]
manifest = "https://…/manifest.json"
connections = 8   # total across every archive, at most 16
```

- Before boot, each enabled index whose directory is missing or empty is downloaded from its
  archive in the manifest. An index holding data is never touched.
- Free space is checked per filesystem before any download. `aria2c` (on `PATH`; shipped in the
  image) downloads and checks each archive's sha256, then it is unpacked beside its index and
  renamed into place.
- A restart resumes each archive at the step it stopped in. SIGTERM / SIGINT during the download
  stops `aria2c` and exits 0.
- Progress shows as state `provisioning` with its bytes as `detail` on `/readyz` and `/status`.

/// The parsed `relman.toml` — placeholder for slice 0.
///
/// Will grow into the typed view of relman's governed targets and options
/// (`[options]` + `[[target]]`), parsed from the repo-committed manifest. Kept
/// intentionally empty for now so the hexagon topology compiles without
/// committing to a schema before the versioning slice lands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseConfig {}

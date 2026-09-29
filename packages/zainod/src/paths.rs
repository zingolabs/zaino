//! Default on-disk locations (`dirs` = XDG base dirs on Linux, platform dirs elsewhere).

use std::path::PathBuf;

/// `<config dir>/zaino/zainod.toml`
pub fn default_config() -> PathBuf {
    under(dirs::config_dir(), ".config").join("zaino/zainod.toml")
}

/// `<cache dir>/zaino/indexes/<name>`
///
/// One directory per index, because they do not share a storage engine — compact blocks are
/// append-only files, not a table in someone else's database.
pub fn default_index(name: &str) -> PathBuf {
    under(dirs::cache_dir(), ".cache").join("zaino/indexes").join(name)
}

/// `/tmp/zaino/<home_subdir>` when no home dir (e.g. unset `HOME` under a service manager)
fn under(base: Option<PathBuf>, home_subdir: &str) -> PathBuf {
    base.unwrap_or_else(|| PathBuf::from("/tmp/zaino").join(home_subdir))
}

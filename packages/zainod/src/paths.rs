//! Default on-disk locations (`dirs` = XDG base dirs on Linux, platform dirs elsewhere).

use std::path::PathBuf;

/// `<config dir>/zaino/zainod.toml`
pub fn default_config() -> PathBuf {
    under(dirs::config_dir(), ".config").join("zaino/zainod.toml")
}

/// `<cache dir>/zaino/store`
pub fn default_store() -> PathBuf {
    under(dirs::cache_dir(), ".cache").join("zaino/store")
}

/// `/tmp/zaino/<home_subdir>` when no home dir (e.g. unset `HOME` under a service manager)
fn under(base: Option<PathBuf>, home_subdir: &str) -> PathBuf {
    base.unwrap_or_else(|| PathBuf::from("/tmp/zaino").join(home_subdir))
}

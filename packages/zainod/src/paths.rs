//! Default on-disk locations (`dirs` = XDG base dirs on Linux, platform dirs elsewhere)

use std::path::PathBuf;

use zaino_persistence::IndexKind;

/// `<config dir>/zaino/zainod.toml`
pub fn default_config() -> PathBuf {
    under(dirs::config_dir(), ".config").join("zaino/zainod.toml")
}

/// `<cache dir>/zaino/indexes/<kind.name()>` (one directory per index: each its own store)
pub(crate) fn default_index(kind: IndexKind) -> PathBuf {
    under(dirs::cache_dir(), ".cache").join("zaino/indexes").join(kind.name())
}

/// `<cache dir>/zaino/peers`: zebra-network's peer address cache
pub(crate) fn default_peer_cache() -> PathBuf {
    under(dirs::cache_dir(), ".cache").join("zaino/peers")
}

/// `/tmp/zaino/<home_subdir>` when no home dir (e.g. unset `HOME` under a service manager)
fn under(base: Option<PathBuf>, home_subdir: &str) -> PathBuf {
    base.unwrap_or_else(|| PathBuf::from("/tmp/zaino").join(home_subdir))
}

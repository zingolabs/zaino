#![forbid(unsafe_code)]

use std::path::Path;

use serde::Serialize;

/// Setting this environment variable rewrites each golden file from the value under test instead of comparing against it.
const BLESS: &str = "ZAINO_BLESS_JSONRPC_GOLDENS";

/// Asserts that `value` serializes to the golden file `dir/name.json`.
pub fn assert_golden<T: Serialize>(dir: &Path, name: &str, value: &T) {
    let path = dir.join(format!("{name}.json"));
    let mut rendered = serde_json::to_string_pretty(value).expect("every golden value serializes");
    rendered.push('\n');

    if std::env::var_os(BLESS).is_some() {
        std::fs::create_dir_all(dir).expect("the golden directory is creatable");
        std::fs::write(&path, &rendered).expect("the golden file is writable");
        return;
    }

    let golden = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "golden {} is missing ({error}); set {BLESS} to capture it",
            path.display()
        )
    });
    assert_eq!(
        rendered, golden,
        "{name} no longer serializes to its golden"
    );
}

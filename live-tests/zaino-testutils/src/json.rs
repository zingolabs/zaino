//! JSON-RPC response comparison helpers shared by the parity tests.
//!
//! Each comparison comes in two forms: a `json_*` function returning
//! `Result<()>`, and an `assert_json_*` wrapper that panics on the `Err`.
//!
//! The `Result` form is the composable one, and exists because a caller that
//! runs many comparisons against one expensively-built topology cannot afford
//! for the first mismatch to unwind the rest — see `clientless/tests/
//! testnet_parity.rs`, which reports every check's verdict together, having
//! paid for its topology once. The `assert_` form stays for single-comparison
//! call sites,
//! where a panic is the clearest thing that can happen.

use anyhow::{ensure, Result};
use serde_json::Value;

/// Assert two JSON-RPC responses agree, after removing fields named by
/// `volatile` paths (dot-separated). Removal (not zeroing) handles the
/// common parity case where one side emits a field zaino mirrors as
/// `null` and the other side omits it entirely.
///
/// Numbers compare by value, not representation: `1` and `1.0` are equal.
/// zaino's typed wire structs promote integer-valued fields (`difficulty`,
/// `relayfee`, …) to `f64`, so the indexer re-serializes them as `1.0`
/// while the validator emits `1`. The pre-ztest tests compared two
/// zaino-deserialized structs and so never saw this; raw-JSON equality
/// would. Treating numerically-equal numbers as equal restores the
/// upstream comparison's behaviour.
pub fn json_equal_ignoring(
    label: &str,
    mut a: Value,
    mut b: Value,
    volatile: &[&str],
) -> Result<()> {
    for path in volatile {
        remove_path(&mut a, path);
        remove_path(&mut b, path);
    }
    ensure!(
        json_eq_numeric(&a, &b),
        "[{label}] validator and indexer JSON-RPC responses disagree \
         (after dropping {volatile:?})\n  left (validator): {a}\n  right (indexer): {b}"
    );
    Ok(())
}

/// Panicking form of [`json_equal_ignoring`].
pub fn assert_json_equal_ignoring(label: &str, a: Value, b: Value, volatile: &[&str]) {
    if let Err(e) = json_equal_ignoring(label, a, b, volatile) {
        panic!("{e:#}");
    }
}

/// Structural JSON equality that compares numbers by value rather than
/// representation, so `1 == 1.0`. Recurses through objects and arrays;
/// every other variant falls back to `PartialEq`.
fn json_eq_numeric(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            x == y || matches!((x.as_f64(), y.as_f64()), (Some(p), Some(q)) if p == q)
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, xv)| y.get(k).is_some_and(|yv| json_eq_numeric(xv, yv)))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(xv, yv)| json_eq_numeric(xv, yv))
        }
        _ => a == b,
    }
}

/// Remove the value at a dot-separated `path` from `v`, if present. A
/// missing key, or a scalar encountered before the leaf, is a no-op.
///
/// Two implicit wildcards keep paths free of index/key syntax:
///   - **Arrays**: the remaining path is applied to every element, so
///     `valuePools.monitored` strips `monitored` from each `valuePools`
///     entry.
///   - **`*` segment on an object**: the remaining path is applied to
///     every value, so `upgrades.*.info` strips `info` from every entry of
///     the (arbitrary-keyed) `upgrades` map. A trailing `*` clears the
///     object.
fn remove_path(v: &mut Value, path: &str) {
    match v {
        Value::Array(items) => {
            for item in items {
                remove_path(item, path);
            }
        }
        Value::Object(m) => match path.split_once('.') {
            None if path == "*" => m.clear(),
            None => {
                m.remove(path);
            }
            Some(("*", rest)) => {
                for val in m.values_mut() {
                    remove_path(val, rest);
                }
            }
            Some((head, rest)) => {
                if let Some(next) = m.get_mut(head) {
                    remove_path(next, rest);
                }
            }
        },
        _ => {}
    }
}

/// Canonicalize an array-valued JSON-RPC response by sorting its elements,
/// so set-valued results compare regardless of the order each source
/// happened to emit them in.
///
/// Necessary because zaino's state backend derives results from its own
/// index while the validator walks its own storage: the *contents* are the
/// contract, the order is not. (`getrawmempool` already needed this and was
/// sorted inline at the call site; the address-index queries need the same
/// treatment over real history, where results are long enough that an
/// order difference is likely rather than theoretical.)
///
/// Non-arrays pass through unchanged, so this is safe to apply to a
/// response that may legitimately be an error object or a scalar. Sorting
/// is by serialized form, which is total and stable across runs.
pub fn sort_json_array(v: Value) -> Value {
    match v {
        Value::Array(mut items) => {
            items.sort_by_key(|item| item.to_string());
            Value::Array(items)
        }
        other => other,
    }
}

/// Light parity check between two JSON-RPC objects. After dropping
/// `ignore` paths from both sides, asserts:
///   - both are JSON objects
///   - their top-level key sets are equal (and thus equal in count)
///   - for each name in `check_values`, the values are byte-equal
///
/// For fields where validator and indexer disagree on numeric
/// representation (e.g. zaino's typed wire structs promote `difficulty`
/// to f64 while zcashd emits an int), don't include them in
/// `check_values` — assert those at the call site with `.as_f64()` on
/// both sides.
pub fn json_shape_matches(
    label: &str,
    mut a: Value,
    mut b: Value,
    ignore: &[&str],
    check_values: &[&str],
) -> Result<()> {
    for path in ignore {
        remove_path(&mut a, path);
        remove_path(&mut b, path);
    }
    let (Value::Object(a_obj), Value::Object(b_obj)) = (&a, &b) else {
        anyhow::bail!("[{label}] expected JSON objects, got validator={a}, indexer={b}");
    };
    let a_keys: std::collections::BTreeSet<&String> = a_obj.keys().collect();
    let b_keys: std::collections::BTreeSet<&String> = b_obj.keys().collect();
    ensure!(
        a_keys == b_keys,
        "[{label}] key set differs: validator={a_keys:?}, indexer={b_keys:?}"
    );
    for name in check_values {
        ensure!(
            a_obj.get(*name) == b_obj.get(*name),
            "[{label}] field {name:?} differs: validator={:?}, indexer={:?}",
            a_obj.get(*name),
            b_obj.get(*name)
        );
    }
    Ok(())
}

/// Panicking form of [`json_shape_matches`].
pub fn assert_json_shape_matches(
    label: &str,
    a: Value,
    b: Value,
    ignore: &[&str],
    check_values: &[&str],
) {
    if let Err(e) = json_shape_matches(label, a, b, ignore, check_values) {
        panic!("{e:#}");
    }
}

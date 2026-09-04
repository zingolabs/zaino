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
/// while the validator emits `1`.
///
/// A `volatile` path that matches nothing on either side is an error: it is
/// either a typo or dead, and both silently weaken the comparison.
pub fn json_equal_ignoring(
    label: &str,
    mut a: Value,
    mut b: Value,
    volatile: &[&str],
) -> Result<()> {
    for path in volatile {
        let hit_a = remove_path(&mut a, path);
        let hit_b = remove_path(&mut b, path);
        ensure!(
            hit_a || hit_b,
            "[{label}] volatile path {path:?} matched nothing on either side\n  \
             left (validator): {a}\n  right (indexer): {b}"
        );
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
///
/// Two integers compare as integers. `f64` only enters when a side is
/// genuinely a float, so distinct integers above 2^53 stay distinct.
fn json_eq_numeric(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_u64(), y.as_u64()) {
            (Some(p), Some(q)) => p == q,
            _ => match (x.as_i64(), y.as_i64()) {
                (Some(p), Some(q)) => p == q,
                _ => matches!((x.as_f64(), y.as_f64()), (Some(p), Some(q)) if p == q),
            },
        },
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

/// Remove the value at a dot-separated `path` from `v`, reporting whether
/// anything was removed. A missing key, or a scalar encountered before the
/// leaf, is a no-op.
///
/// A path only ever removes at the level it names. Two implicit wildcards
/// keep paths free of index/key syntax:
///   - **Arrays**: the remaining path is applied to every object element,
///     so `valuePools.monitored` strips `monitored` from each `valuePools`
///     entry. Transparency does not stack — elements that are themselves
///     arrays are left alone, so the path cannot reach deeper than the
///     level it names.
///   - **`*` segment on an object**: the remaining path is applied to
///     every value, so `upgrades.*.info` strips `info` from every entry of
///     the (arbitrary-keyed) `upgrades` map. A trailing `*` clears the
///     object.
fn remove_path(v: &mut Value, path: &str) -> bool {
    // Every branch visits all candidates: `any` would stop at the first hit
    // and leave the rest of the volatile field in place.
    match v {
        Value::Array(items) => {
            let mut hit = false;
            for item in items.iter_mut().filter(|item| item.is_object()) {
                hit |= remove_path(item, path);
            }
            hit
        }
        Value::Object(m) => match path.split_once('.') {
            None if path == "*" => {
                let hit = !m.is_empty();
                m.clear();
                hit
            }
            None => m.remove(path).is_some(),
            Some(("*", rest)) => {
                let mut hit = false;
                for val in m.values_mut() {
                    hit |= remove_path(val, rest);
                }
                hit
            }
            Some((head, rest)) => m.get_mut(head).is_some_and(|next| remove_path(next, rest)),
        },
        _ => false,
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
/// to f64 while the validator emits an int), don't include them in
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
        // Absent on both sides would otherwise compare equal, so a typo'd or
        // already-ignored name would pass vacuously.
        let (Some(av), Some(bv)) = (a_obj.get(*name), b_obj.get(*name)) else {
            let missing = match (a_obj.contains_key(*name), b_obj.contains_key(*name)) {
                (false, false) => "neither validator nor indexer",
                (false, true) => "validator",
                _ => "indexer",
            };
            anyhow::bail!("[{label}] checked field {name:?} is absent from {missing}");
        };
        ensure!(
            av == bv,
            "[{label}] field {name:?} differs: validator={av}, indexer={bv}"
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Above 2^53 an f64 comparison collapses adjacent integers into one.
    #[test]
    fn large_distinct_integers_are_unequal() {
        let a = json!({ "n": 9007199254740993u64 });
        let b = json!({ "n": 9007199254740992u64 });
        assert!(json_equal_ignoring("n", a, b, &[]).is_err());
    }

    #[test]
    fn int_and_float_of_the_same_value_are_equal() {
        let a = json!({ "difficulty": 1 });
        let b = json!({ "difficulty": 1.0 });
        json_equal_ignoring("difficulty", a, b, &[]).unwrap();
    }

    #[test]
    fn a_path_removes_only_at_the_level_it_names() {
        let mut v = json!({ "errors": "top", "nested": { "errors": "deep" } });
        assert!(remove_path(&mut v, "errors"));
        assert_eq!(v, json!({ "nested": { "errors": "deep" } }));
    }

    #[test]
    fn array_transparency_does_not_stack() {
        let mut v = json!({ "pools": [{ "id": 1 }, [{ "id": 2 }]] });
        assert!(remove_path(&mut v, "pools.id"));
        assert_eq!(v, json!({ "pools": [{}, [{ "id": 2 }]] }));
    }

    #[test]
    fn a_volatile_path_matching_nothing_is_an_error() {
        let err = json_equal_ignoring("getinfo", json!({ "a": 1 }), json!({ "a": 1 }), &["typo"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched nothing"), "unexpected: {err}");
    }

    /// One-sided presence is a real parity difference, so removal there still
    /// counts as the path firing.
    #[test]
    fn a_volatile_path_matching_one_side_is_accepted() {
        json_equal_ignoring(
            "getinfo",
            json!({ "a": 1, "b": 2 }),
            json!({ "a": 1 }),
            &["b"],
        )
        .unwrap();
    }

    #[test]
    fn a_checked_field_absent_from_both_sides_fails() {
        let err = json_shape_matches("getinfo", json!({}), json!({}), &[], &["blocks"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("absent from neither"), "unexpected: {err}");
    }
}

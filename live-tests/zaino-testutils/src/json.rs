//! JSON-RPC response comparison helpers shared by the parity tests.

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
pub fn assert_json_equal_ignoring(label: &str, mut a: Value, mut b: Value, volatile: &[&str]) {
    for path in volatile {
        remove_path(&mut a, path);
        remove_path(&mut b, path);
    }
    assert!(
        json_eq_numeric(&a, &b),
        "[{label}] validator and indexer JSON-RPC responses disagree \
         (after dropping {volatile:?})\n  left (validator): {a}\n  right (indexer): {b}"
    );
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
pub fn assert_json_shape_matches(
    label: &str,
    mut a: Value,
    mut b: Value,
    ignore: &[&str],
    check_values: &[&str],
) {
    for path in ignore {
        remove_path(&mut a, path);
        remove_path(&mut b, path);
    }
    let (Value::Object(a_obj), Value::Object(b_obj)) = (&a, &b) else {
        panic!("[{label}] expected JSON objects, got validator={a}, indexer={b}");
    };
    let a_keys: std::collections::BTreeSet<&String> = a_obj.keys().collect();
    let b_keys: std::collections::BTreeSet<&String> = b_obj.keys().collect();
    assert_eq!(
        a_keys, b_keys,
        "[{label}] key set differs: validator={a_keys:?}, indexer={b_keys:?}"
    );
    for name in check_values {
        assert_eq!(
            a_obj.get(*name),
            b_obj.get(*name),
            "[{label}] field {name:?} differs"
        );
    }
}

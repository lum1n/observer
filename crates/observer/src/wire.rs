//! Small helpers for reading harness JSON.

use serde_json::Value;

/// Upper bound for one JSONL record. Transcript lines can be large, but
/// nothing legitimate comes close.
pub(crate) const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(crate) fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn owned(v: &Value, key: &str) -> Option<String> {
    str_at(v, key).map(str::to_owned)
}

pub(crate) fn u64_at(v: &Value, key: &str) -> Option<u64> {
    let x = v.get(key)?;
    x.as_u64()
        .or_else(|| x.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

/// Sums the given integer fields, treating missing ones as zero. `None` if all are missing.
pub(crate) fn sum(v: &Value, keys: &[&str]) -> Option<u64> {
    let mut any = false;
    let mut total = 0u64;
    for k in keys {
        if let Some(n) = u64_at(v, k) {
            any = true;
            total = total.saturating_add(n);
        }
    }
    any.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sum_and_numbers() {
        let v = json!({"a": 1, "b": 2.0, "c": "x"});
        assert_eq!(sum(&v, &["a", "b", "z"]), Some(3));
        assert_eq!(sum(&v, &["z"]), None);
        assert_eq!(u64_at(&v, "c"), None);
    }
}

//! Small helpers the structural diffs share.

use std::collections::{BTreeMap, BTreeSet};

/// A keyed map [`diff_maps`] can walk: the diffs read both [`BTreeMap`]s and
/// `serde_json` objects, and neither is a view of the other.
pub(crate) trait DiffMap {
    type Key: Ord;
    type Value: PartialEq;
    fn diff_keys(&self) -> impl Iterator<Item = &Self::Key>;
    fn diff_get(&self, k: &Self::Key) -> Option<&Self::Value>;
}

impl<K: Ord, V: PartialEq> DiffMap for BTreeMap<K, V> {
    type Key = K;
    type Value = V;
    fn diff_keys(&self) -> impl Iterator<Item = &K> {
        self.keys()
    }
    fn diff_get(&self, k: &K) -> Option<&V> {
        self.get(k)
    }
}

impl DiffMap for serde_json::Map<String, serde_json::Value> {
    type Key = String;
    type Value = serde_json::Value;
    fn diff_keys(&self) -> impl Iterator<Item = &String> {
        self.keys()
    }
    fn diff_get(&self, k: &String) -> Option<&serde_json::Value> {
        self.get(k)
    }
}

/// Every key on either side whose value differs — a key present on one side
/// only counts — in key order, with the value each side has for it.
///
/// This is the one loop every diff in this crate is made of: `MODULE.bazel`
/// against `MODULE.bazel`, lock against lock, `WORKSPACE` against
/// `WORKSPACE`. Key order is what makes the first difference found, and so
/// the reason a run reports, deterministic.
pub(crate) fn diff_maps<'a, M: DiffMap>(
    a: &'a M,
    b: &'a M,
) -> impl Iterator<Item = (&'a M::Key, Option<&'a M::Value>, Option<&'a M::Value>)> + 'a {
    let keys: BTreeSet<&'a M::Key> = a.diff_keys().chain(b.diff_keys()).collect();
    keys.into_iter().filter_map(move |k| {
        let (x, y) = (a.diff_get(k), b.diff_get(k));
        (x != y).then_some((k, x, y))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yields_every_difference_once_in_key_order() {
        let a: BTreeMap<&str, u32> = [("b", 1), ("c", 2), ("d", 3)].into_iter().collect();
        let b: BTreeMap<&str, u32> = [("a", 0), ("b", 1), ("c", 9)].into_iter().collect();
        let got: Vec<_> = diff_maps(&a, &b).collect();
        assert_eq!(
            got,
            vec![
                (&"a", None, Some(&0)),
                (&"c", Some(&2), Some(&9)),
                (&"d", Some(&3), None),
            ]
        );
        assert_eq!(diff_maps(&a, &a).count(), 0);
    }

    #[test]
    fn walks_json_objects_too() {
        let a: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str("{\"x\": 1, \"y\": 2}").unwrap();
        let b: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str("{\"x\": 1, \"y\": 3}").unwrap();
        let got: Vec<_> = diff_maps(&a, &b).map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(got, ["y"]);
    }
}

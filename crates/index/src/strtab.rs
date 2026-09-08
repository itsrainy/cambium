//! Append-only string table plus the lookup map rebuilt on load.
//!
//! The table itself is two flat arrays (`bytes`, `offsets`) so it can be
//! serialised as-is and addressed straight out of an mmap. The `label -> id`
//! map is *not* serialised: it is rebuilt lazily, and only the code paths that
//! need to map a name back to an id (splice, file lookup) pay for it. A pure
//! walk never touches it.

use rustc_hash::FxHashMap;

/// Borrowed view over a serialised string table.
#[derive(Clone, Copy)]
pub struct StrTabRef<'a> {
    pub bytes: &'a [u8],
    /// `offsets[i]..offsets[i + 1]` is entry `i`; length is `len + 1`.
    pub offsets: &'a [u32],
}

impl<'a> StrTabRef<'a> {
    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entry `i`. Panics if `i` is out of range.
    #[inline]
    pub fn get(&self, i: u32) -> &'a str {
        let s = self.offsets[i as usize] as usize;
        let e = self.offsets[i as usize + 1] as usize;
        let raw = &self.bytes[s..e];
        // SAFETY: `validate` has already checked, once, that `bytes` is UTF-8
        // and that every offset lands on a character boundary — a table built
        // in-process comes from `Interner`, which only ever appends a `&str`,
        // and one read off disk goes through `validate` at open. The slicing
        // above is still bounds-checked, so a bad `i` panics rather than
        // reading out of range. Doing the UTF-8 check here instead costs a
        // scan of the label on every id-to-name lookup, which a walk does
        // millions of times.
        unsafe { std::str::from_utf8_unchecked(raw) }
    }

    /// Checks the invariants [`StrTabRef::get`] relies on: offsets ascending,
    /// in range, ending at `bytes.len()`, every one of them a character
    /// boundary of a valid UTF-8 `bytes`.
    ///
    /// O(bytes + len), run once when a file is opened rather than per lookup.
    pub fn validate(&self, what: &str) -> std::result::Result<(), String> {
        if self.offsets.is_empty() {
            return Err(format!(
                "{what}: offsets array is empty (want at least [0])"
            ));
        }
        if self.offsets[0] != 0 {
            return Err(format!("{what}: offsets do not start at 0"));
        }
        if *self.offsets.last().unwrap() as usize != self.bytes.len() {
            return Err(format!(
                "{what}: last offset {} does not match the {}-byte table",
                self.offsets.last().unwrap(),
                self.bytes.len()
            ));
        }
        let text = std::str::from_utf8(self.bytes).map_err(|e| format!("{what}: {e}"))?;
        for w in self.offsets.windows(2) {
            if w[1] < w[0] {
                return Err(format!("{what}: offsets {} -> {} descend", w[0], w[1]));
            }
            if !text.is_char_boundary(w[0] as usize) {
                return Err(format!(
                    "{what}: offset {} is not a character boundary",
                    w[0]
                ));
            }
        }
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.len() as u32).map(move |i| self.get(i))
    }

    /// Rebuilds the `entry -> id` map. O(len); ~50 ms at 1M labels.
    pub fn build_map(&self) -> FxHashMap<&'a str, u32> {
        let mut m = FxHashMap::with_capacity_and_hasher(self.len(), rustc_hash::FxBuildHasher);
        for i in 0..self.len() as u32 {
            m.insert(self.get(i), i);
        }
        m
    }
}

/// Owned, growable string table with an interning map.
///
/// The map is a `hashbrown::HashTable<u32>` keyed by hash rather than a
/// `HashMap<Box<str>, u32>`: entries point back into `bytes`, so a label is
/// stored exactly once. At a million labels that is 40 MB instead of 90 MB and
/// a million fewer allocations.
pub struct Interner {
    bytes: Vec<u8>,
    offsets: Vec<u32>,
    table: hashbrown::HashTable<u32>,
}

impl Default for Interner {
    /// `#[derive(Default)]` would leave `offsets` empty instead of the
    /// single leading `0` every entry range needs; go through `new()` so the
    /// invariant `offsets.len() == len() + 1` always holds.
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Interner {
    fn clone(&self) -> Self {
        Self::from_parts(self.bytes.clone(), self.offsets.clone())
    }
}

#[inline]
fn hash_str(s: &str) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = rustc_hash::FxBuildHasher.build_hasher();
    h.write(s.as_bytes());
    h.finish()
}

#[inline]
fn entry_at<'a>(bytes: &'a [u8], offsets: &[u32], i: u32) -> &'a str {
    let s = offsets[i as usize] as usize;
    let e = offsets[i as usize + 1] as usize;
    std::str::from_utf8(&bytes[s..e]).expect("string table holds UTF-8")
}

impl Interner {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            offsets: vec![0],
            table: hashbrown::HashTable::new(),
        }
    }

    /// Rebuilds an interner from serialised arrays, preserving ids.
    pub fn from_parts(bytes: Vec<u8>, offsets: Vec<u32>) -> Self {
        let n = offsets.len() - 1;
        let mut table = hashbrown::HashTable::with_capacity(n);
        for i in 0..n as u32 {
            let h = hash_str(entry_at(&bytes, &offsets, i));
            table.insert_unique(h, i, |&other| hash_str(entry_at(&bytes, &offsets, other)));
        }
        Self {
            bytes,
            offsets,
            table,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn get(&self, i: u32) -> &str {
        entry_at(&self.bytes, &self.offsets, i)
    }

    #[inline]
    pub fn lookup(&self, s: &str) -> Option<u32> {
        let h = hash_str(s);
        self.table
            .find(h, |&id| entry_at(&self.bytes, &self.offsets, id) == s)
            .copied()
    }

    /// Interns `s`, returning its existing id or appending a new one.
    pub fn intern(&mut self, s: &str) -> u32 {
        self.intern_new(s).0
    }

    /// Interns `s` and reports whether it was newly added.
    pub fn intern_new(&mut self, s: &str) -> (u32, bool) {
        let Self {
            bytes,
            offsets,
            table,
        } = self;
        let h = hash_str(s);
        if let Some(&id) = table.find(h, |&id| entry_at(bytes, offsets, id) == s) {
            return (id, false);
        }
        let id = (offsets.len() - 1) as u32;
        bytes.extend_from_slice(s.as_bytes());
        offsets.push(bytes.len() as u32);
        table.insert_unique(h, id, |&other| hash_str(entry_at(bytes, offsets, other)));
        (id, true)
    }

    pub fn as_ref(&self) -> StrTabRef<'_> {
        StrTabRef {
            bytes: &self.bytes,
            offsets: &self.offsets,
        }
    }

    /// Consumes the interner, yielding the serialisable arrays.
    pub fn into_parts(self) -> (Vec<u8>, Vec<u32>) {
        (self.bytes, self.offsets)
    }

    pub fn parts(&self) -> (&[u8], &[u32]) {
        (&self.bytes, &self.offsets)
    }

    /// Bytes held by the table itself, for memory reporting.
    pub fn byte_size(&self) -> usize {
        self.bytes.len() + self.offsets.len() * 4 + self.table.capacity() * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_and_preserves_ids() {
        let mut i = Interner::new();
        assert_eq!(i.intern("//a:b"), 0);
        assert_eq!(i.intern("//c:d"), 1);
        assert_eq!(i.intern("//a:b"), 0);
        assert_eq!(i.get(1), "//c:d");
        assert_eq!(i.intern_new("//e:f"), (2, true));
        assert_eq!(i.intern_new("//e:f"), (2, false));
    }

    #[test]
    fn round_trips_through_parts() {
        let mut i = Interner::new();
        for s in ["//a:b", "//c:d", "//e:f"] {
            i.intern(s);
        }
        let (b, o) = i.clone().into_parts();
        let j = Interner::from_parts(b, o);
        assert_eq!(j.len(), 3);
        assert_eq!(j.lookup("//c:d"), Some(1));
        assert_eq!(j.get(2), "//e:f");
    }

    #[test]
    fn default_matches_new() {
        // `#[derive(Default)]` would give `offsets == []`, which makes `len()`
        // underflow; `Default` must go through `new()`.
        let d = Interner::default();
        assert_eq!(d.len(), 0);
        assert!(d.is_empty());
        assert_eq!(d.as_ref().len(), 0);
        let mut d = d;
        assert_eq!(d.intern("//a:b"), 0);
        assert_eq!(d.get(0), "//a:b");
    }

    #[test]
    fn validate_accepts_a_real_table_and_rejects_corruption() {
        let mut i = Interner::new();
        for s in ["//a:b", "héllo", "//c:d"] {
            i.intern(s);
        }
        let (bytes, offsets) = i.into_parts();
        assert!(StrTabRef {
            bytes: &bytes,
            offsets: &offsets
        }
        .validate("t")
        .is_ok());

        // An offset in the middle of the two-byte 'é'.
        let mut bad = offsets.clone();
        bad[1] += 2;
        assert!(StrTabRef {
            bytes: &bytes,
            offsets: &bad
        }
        .validate("t")
        .is_err());

        // Bytes that are not UTF-8 at all.
        assert!(StrTabRef {
            bytes: &[0xff, 0xfe],
            offsets: &[0, 2]
        }
        .validate("t")
        .is_err());

        // A last offset that does not cover the table.
        assert!(StrTabRef {
            bytes: &bytes,
            offsets: &offsets[..offsets.len() - 1]
        }
        .validate("t")
        .is_err());

        // No offsets at all.
        assert!(StrTabRef {
            bytes: &[],
            offsets: &[]
        }
        .validate("t")
        .is_err());
    }

    #[test]
    fn ref_builds_map() {
        let mut i = Interner::new();
        i.intern("x");
        i.intern("y");
        let m = i.as_ref().build_map();
        assert_eq!(m["y"], 1);
    }
}

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
        // Safety-by-construction: every entry was pushed from a &str and the
        // table is immutable once written.
        std::str::from_utf8(&self.bytes[s..e]).expect("string table holds UTF-8")
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.len() as u32).map(move |i| self.get(i))
    }

    /// Rebuilds the `entry -> id` map. O(len); ~50 ms at 1M labels.
    pub fn build_map(&self) -> FxHashMap<&'a str, u32> {
        let mut m =
            FxHashMap::with_capacity_and_hasher(self.len(), rustc_hash::FxBuildHasher);
        for i in 0..self.len() as u32 {
            m.insert(self.get(i), i);
        }
        m
    }
}

/// Owned, growable string table with an interning map.
#[derive(Default, Clone)]
pub struct Interner {
    bytes: Vec<u8>,
    offsets: Vec<u32>,
    map: FxHashMap<Box<str>, u32>,
}

impl Interner {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            offsets: vec![0],
            map: FxHashMap::default(),
        }
    }

    /// Rebuilds an interner from serialised arrays, preserving ids.
    pub fn from_parts(bytes: Vec<u8>, offsets: Vec<u32>) -> Self {
        let mut me = Self {
            bytes,
            offsets,
            map: FxHashMap::default(),
        };
        let n = me.len();
        me.map = FxHashMap::with_capacity_and_hasher(n, rustc_hash::FxBuildHasher);
        for i in 0..n as u32 {
            let s: Box<str> = me.get(i).into();
            me.map.insert(s, i);
        }
        me
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
        let s = self.offsets[i as usize] as usize;
        let e = self.offsets[i as usize + 1] as usize;
        std::str::from_utf8(&self.bytes[s..e]).expect("string table holds UTF-8")
    }

    #[inline]
    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.map.get(s).copied()
    }

    /// Interns `s`, returning its existing id or appending a new one.
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        self.offsets.push(self.bytes.len() as u32);
        self.map.insert(s.into(), id);
        id
    }

    /// Interns `s` and reports whether it was newly added.
    pub fn intern_new(&mut self, s: &str) -> (u32, bool) {
        let before = self.len();
        let id = self.intern(s);
        (id, id as usize == before)
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
    fn ref_builds_map() {
        let mut i = Interner::new();
        i.intern("x");
        i.intern("y");
        let m = i.as_ref().build_map();
        assert_eq!(m["y"], 1);
    }
}

//! Repository names, interned: a [`RepoId`] per distinct name, and
//! [`Repos`], a set of them as a bitset.
//!
//! The rules union sets thousands of times per closure, and a fallback set
//! — every repository the main repository defines — is thousands of names
//! wide. As `BTreeSet<String>` each union cloned every name; as a bitset a
//! union is one word-wise `or` and a clone is a `memcpy`. The names come
//! back as strings only at the boundary, in [`super::Attributed`].

use fixedbitset::FixedBitSet;
use rustc_hash::FxHashMap;

/// One distinct repository name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RepoId(u32);

/// Name <-> id, append-only.
#[derive(Default)]
pub(crate) struct Interner {
    ids: FxHashMap<String, RepoId>,
    names: Vec<String>,
}

impl Interner {
    pub(crate) fn intern(&mut self, name: &str) -> RepoId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = RepoId(self.names.len() as u32);
        self.ids.insert(name.to_string(), id);
        self.names.push(name.to_string());
        id
    }

    pub(crate) fn name(&self, id: RepoId) -> &str {
        &self.names[id.0 as usize]
    }

    /// The names of a set, sorted.
    pub(crate) fn names(&self, set: &Repos) -> std::collections::BTreeSet<String> {
        set.iter().map(|id| self.name(id).to_string()).collect()
    }

    /// A set of the given names, interning any not yet seen.
    pub(crate) fn set<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) -> Repos {
        let mut out = Repos::new();
        for n in names {
            out.insert(self.intern(n));
        }
        out
    }
}

/// A set of repositories. Grows as ids are inserted, so a set made before
/// the interner saw a name can still hold it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Repos(FixedBitSet);

impl Repos {
    pub(crate) fn new() -> Self {
        Repos(FixedBitSet::new())
    }

    pub(crate) fn single(id: RepoId) -> Self {
        let mut out = Repos::new();
        out.insert(id);
        out
    }

    pub(crate) fn insert(&mut self, id: RepoId) {
        let bit = id.0 as usize;
        if bit >= self.0.len() {
            self.0.grow(bit + 1);
        }
        self.0.insert(bit);
    }

    /// Adds every member of `other`; whether that added anything.
    pub(crate) fn union_with(&mut self, other: &Repos) -> bool {
        if other.is_subset_of(self) {
            return false;
        }
        self.0.union_with(&other.0);
        true
    }

    fn is_subset_of(&self, other: &Repos) -> bool {
        self.iter().all(|id| other.contains(id))
    }

    pub(crate) fn contains(&self, id: RepoId) -> bool {
        self.0.contains(id.0 as usize)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_clear()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = RepoId> + '_ {
        self.0.ones().map(|b| RepoId(b as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_grow_and_union_reports_growth() {
        let mut i = Interner::default();
        let a = i.intern("a");
        let b = i.intern("b");
        assert_eq!(i.intern("a"), a);
        let mut s = Repos::single(a);
        assert!(!s.union_with(&Repos::single(a)));
        let c = i.intern("c");
        assert!(s.union_with(&Repos::single(c)));
        assert!(s.contains(c) && !s.contains(b));
        assert_eq!(i.names(&s), ["a".to_string(), "c".into()].into());
        assert!(Repos::new().is_empty() && !s.is_empty());
        // A wider set unioned into a narrower one, and the reverse.
        let mut t = Repos::single(b);
        assert!(t.union_with(&s));
        assert!(!s.clone().union_with(&Repos::new()));
        assert_eq!(i.set(["b", "d"]).iter().count(), 2);
    }
}

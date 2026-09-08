//! Label and node storage that a builder can grow without copying its base.
//!
//! A cold build has no base, so everything lives in the `new` vectors and the
//! cost is what it always was. A splice has a base it must not copy — that copy
//! is 52% of a pull-request run — so existing entries are read straight from
//! the mmapped index and only what the splice actually touches is stored.
//!
//! Two finalizers sit on top of this: [`crate::IndexBuilder::finish`]
//! materialises a complete new index (for `baseline update`, whose output gets
//! published), and [`crate::IndexBuilder::finish_overlay`] produces only the
//! delta (for `affected`, which just needs to walk).

use rustc_hash::FxHashMap;

use crate::data::{IndexRef, NO_ID};
use crate::strtab::{Interner, StrTabRef};

/// Everything the node columns hold for one node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeMeta {
    pub kind: u8,
    pub package: u32,
    pub class: u32,
    pub hash: [u8; 32],
    pub is_test: bool,
    pub flags: u8,
}

impl NodeMeta {
    /// A node for a label some rule references but no queried package
    /// defines: an external repository's target, or one in a package that
    /// failed to load.
    pub fn unknown() -> Self {
        Self {
            kind: crate::data::NodeKind::Unknown as u8,
            package: NO_ID,
            class: NO_ID,
            hash: [0; 32],
            is_test: false,
            flags: 0,
        }
    }
}

/// Labels: an immutable base table searched by binary search, plus the labels
/// this generation appended.
pub struct LabelSpace<'a> {
    base: Option<StrTabRef<'a>>,
    /// Base ids ordered by label.
    base_sorted: &'a [u32],
    base_n: u32,
    new: Interner,
}

impl<'a> LabelSpace<'a> {
    pub fn empty() -> Self {
        Self {
            base: None,
            base_sorted: &[],
            base_n: 0,
            new: Interner::new(),
        }
    }

    pub fn over(base: StrTabRef<'a>, base_sorted: &'a [u32]) -> Self {
        Self {
            base: Some(base),
            base_sorted,
            base_n: base.len() as u32,
            new: Interner::new(),
        }
    }

    #[inline]
    pub fn base_len(&self) -> u32 {
        self.base_n
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.base_n + self.new.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn get(&self, id: u32) -> &str {
        if id < self.base_n {
            self.base.expect("base ids need a base table").get(id)
        } else {
            self.new.get(id - self.base_n)
        }
    }

    /// O(log n) in the base, O(1) in the appended labels.
    pub fn lookup(&self, label: &str) -> Option<u32> {
        if let Some(base) = self.base {
            if let Ok(i) = self
                .base_sorted
                .binary_search_by(|&id| base.get(id).cmp(label))
            {
                return Some(self.base_sorted[i]);
            }
        }
        self.new.lookup(label).map(|i| i + self.base_n)
    }

    /// Returns the label's id, appending it if it is new.
    pub fn intern_new(&mut self, label: &str) -> (u32, bool) {
        if let Some(id) = self.lookup(label) {
            return (id, false);
        }
        let (i, _) = self.new.intern_new(label);
        (i + self.base_n, true)
    }

    /// Labels appended by this generation, in id order.
    pub fn appended(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.new.len() as u32).map(|i| self.new.get(i))
    }

    /// The full label table for a materialised index, plus the id-by-label
    /// ordering.
    ///
    /// The ordering is produced by merging the base's — labels never change for
    /// an id, so the base's order still holds — with the appended labels sorted
    /// among themselves. That keeps a splice linear rather than re-sorting
    /// millions of strings.
    pub fn finish(self) -> (Vec<u8>, Vec<u32>, Vec<u32>) {
        let (mut bytes, mut offsets) = match self.base {
            Some(b) => (b.bytes.to_vec(), b.offsets.to_vec()),
            None => (Vec::new(), vec![0]),
        };
        let (new_bytes, new_offsets) = self.new.into_parts();
        let shift = *offsets.last().unwrap_or(&0);
        bytes.extend_from_slice(&new_bytes);
        offsets.extend(new_offsets.iter().skip(1).map(|o| o + shift));

        let table = StrTabRef {
            bytes: &bytes,
            offsets: &offsets,
        };
        let n = table.len() as u32;
        let mut appended: Vec<u32> = (self.base_n..n).collect();
        // A cold build sorts every label here, which is the one place this
        // index costs real time; a splice sorts only what it appended.
        if appended.len() > 1 << 16 {
            use rayon::prelude::*;
            appended.par_sort_unstable_by(|&a, &b| table.get(a).cmp(table.get(b)));
        } else {
            appended.sort_unstable_by(|&a, &b| table.get(a).cmp(table.get(b)));
        }

        let mut sorted = Vec::with_capacity(n as usize);
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.base_sorted.len() && j < appended.len() {
            if table.get(self.base_sorted[i]) <= table.get(appended[j]) {
                sorted.push(self.base_sorted[i]);
                i += 1;
            } else {
                sorted.push(appended[j]);
                j += 1;
            }
        }
        sorted.extend_from_slice(&self.base_sorted[i..]);
        sorted.extend_from_slice(&appended[j..]);
        debug_assert_eq!(sorted.len(), n as usize);
        (bytes, offsets, sorted)
    }
}

/// Node columns: read from the base, written into an override map or the
/// appended vector.
pub struct NodeSpace<'a> {
    base: Option<IndexRef<'a>>,
    base_n: u32,
    new: Vec<NodeMeta>,
    over: FxHashMap<u32, NodeMeta>,
}

impl<'a> NodeSpace<'a> {
    pub fn empty() -> Self {
        Self {
            base: None,
            base_n: 0,
            new: Vec::new(),
            over: FxHashMap::default(),
        }
    }

    pub fn over(base: IndexRef<'a>) -> Self {
        Self {
            base: Some(base),
            base_n: base.num_nodes(),
            new: Vec::new(),
            over: FxHashMap::default(),
        }
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.base_n + self.new.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn base_len(&self) -> u32 {
        self.base_n
    }

    pub fn push(&mut self, meta: NodeMeta) {
        self.new.push(meta);
    }

    #[inline]
    pub fn get(&self, id: u32) -> NodeMeta {
        if id >= self.base_n {
            return self.new[(id - self.base_n) as usize];
        }
        if let Some(m) = self.over.get(&id) {
            return *m;
        }
        self.base.expect("base ids need a base view").meta(id)
    }

    #[inline]
    pub fn set(&mut self, id: u32, meta: NodeMeta) {
        if id >= self.base_n {
            self.new[(id - self.base_n) as usize] = meta;
        } else {
            self.over.insert(id, meta);
        }
    }

    /// Ids below the base watermark whose metadata this generation changed.
    pub fn overridden(&self) -> &FxHashMap<u32, NodeMeta> {
        &self.over
    }

    pub fn appended(&self) -> &[NodeMeta] {
        &self.new
    }

    /// Fans the columns out for a materialised index.
    pub fn finish(self) -> Columns {
        let n = self.len() as usize;
        let mut c = match self.base {
            Some(b) => Columns {
                kind: b.kind.to_vec(),
                package: b.package.to_vec(),
                class: b.class.to_vec(),
                hash: b.hash.to_vec(),
                is_test: b.is_test.to_vec(),
                flags: b.flags.to_vec(),
            },
            None => Columns::default(),
        };
        c.reserve(n);
        for (&id, m) in &self.over {
            let i = id as usize;
            c.kind[i] = m.kind;
            c.package[i] = m.package;
            c.class[i] = m.class;
            c.hash[i] = m.hash;
            c.is_test[i] = m.is_test;
            c.flags[i] = m.flags;
        }
        for m in &self.new {
            c.push(m);
        }
        c
    }
}

#[derive(Default)]
pub struct Columns {
    pub kind: Vec<u8>,
    pub package: Vec<u32>,
    pub class: Vec<u32>,
    pub hash: Vec<[u8; 32]>,
    pub is_test: Vec<bool>,
    pub flags: Vec<u8>,
}

impl Columns {
    fn reserve(&mut self, n: usize) {
        self.kind.reserve(n.saturating_sub(self.kind.len()));
        self.package.reserve(n.saturating_sub(self.package.len()));
        self.class.reserve(n.saturating_sub(self.class.len()));
        self.hash.reserve(n.saturating_sub(self.hash.len()));
        self.is_test.reserve(n.saturating_sub(self.is_test.len()));
        self.flags.reserve(n.saturating_sub(self.flags.len()));
    }

    fn push(&mut self, m: &NodeMeta) {
        self.kind.push(m.kind);
        self.package.push(m.package);
        self.class.push(m.class);
        self.hash.push(m.hash);
        self.is_test.push(m.is_test);
        self.flags.push(m.flags);
    }
}

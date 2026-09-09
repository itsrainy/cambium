//! Store-key resolution: which key holds the index for a revision, and where a
//! fetched blob lands on disk.
//!
//! [`BaselineCache`] is the piece of the engine that knows the store layout
//! (`docs/STORE.md`) — lineage-then-tree keys, the `commits/` pointers, the
//! lineage-less fallbacks for stores that cannot be listed — and nothing else:
//! no git, no Bazel, no config beyond the hash an index must match. That is
//! what lets the key resolution be tested against a scripted store without an
//! engine around it.

use std::path::{Path, PathBuf};

use cambium_index::{format, IndexFile};

use crate::error::{EngineError, Result};
use crate::store::ArtifactStore;

/// A store plus the local directory fetched indexes are cached in.
pub struct BaselineCache<'a> {
    store: &'a dyn ArtifactStore,
    cache_dir: &'a Path,
    /// Only an index built with this configuration is a baseline; anything
    /// else in the store is a leftover from another lineage.
    config_hash: [u8; 32],
}

impl<'a> BaselineCache<'a> {
    pub fn new(store: &'a dyn ArtifactStore, cache_dir: &'a Path, config_hash: [u8; 32]) -> Self {
        Self {
            store,
            cache_dir,
            config_hash,
        }
    }

    /// Finds an index for `commit`, whose tree is `tree`, in the store.
    ///
    /// Indexes are keyed by tree hash — `indexes/<lineage>/<tree>.bin` — so any
    /// commit with the same content (a rebase, a squash-merge, a cherry-pick)
    /// hits the same blob. A run does not know the lineage before it has an
    /// index, so a store that can enumerate is asked for the key ending in
    /// this tree; failing that, for the `commits/<lineage>/<sha>` pointer,
    /// which names the tree when git could not. A store that cannot enumerate
    /// (plain HTTP) is asked for the lineage-less fallback keys the publisher
    /// also writes. Keys from the sha-addressed layout this replaces are still
    /// recognised.
    ///
    /// `tree` equals `commit` when git could not resolve it (a synthetic
    /// revision id in the tests, a sha this clone does not have).
    pub fn fetch(&self, commit: &str, tree: &str) -> Result<Option<PathBuf>> {
        std::fs::create_dir_all(self.cache_dir).map_err(EngineError::io_at(self.cache_dir))?;
        let mut suffixes = vec![format!("/{tree}.bin")];
        if tree != commit {
            suffixes.push(format!("/{commit}.bin"));
        }
        suffixes.push(format!("-{commit}.bin"));
        if let Ok(names) = self.store.list() {
            // Several lineages can hold this tree: every `baseline build`
            // mints one, and a config change (or a rule-hash version bump)
            // leaves the old lineage's blobs behind. Take the first one built
            // with this configuration; an incompatible blob is not a baseline,
            // it is a leftover.
            let candidates = names
                .iter()
                .filter(|n| suffixes.iter().any(|s| n.ends_with(s)));
            for name in candidates {
                if let Some(dest) = self.fetch_if_compatible(name)? {
                    return Ok(Some(dest));
                }
            }
            // Tooling that only has a sha: follow the pointer to the tree.
            let pointer_suffix = format!("/{commit}");
            let pointers = names
                .iter()
                .filter(|n| n.starts_with("commits/") && n.ends_with(&pointer_suffix));
            for name in pointers {
                let Some(pointed) = self.read_pointer(name)? else {
                    continue;
                };
                let suffix = format!("/{pointed}.bin");
                for name in names.iter().filter(|n| n.ends_with(&suffix)) {
                    if let Some(dest) = self.fetch_if_compatible(name)? {
                        return Ok(Some(dest));
                    }
                }
            }
            return Ok(None);
        }
        for name in [tree_addressed_key(tree), commit_addressed_key(commit)] {
            if let Some(dest) = self.fetch_if_compatible(&name)? {
                return Ok(Some(dest));
            }
        }
        if let Some(pointed) = self.read_pointer(&commit_addressed_pointer(commit))? {
            if let Some(dest) = self.fetch_if_compatible(&tree_addressed_key(&pointed))? {
                return Ok(Some(dest));
            }
        }
        Ok(None)
    }

    /// The tree a `commits/` pointer names, if the store has the pointer.
    fn read_pointer(&self, name: &str) -> Result<Option<String>> {
        let dest = self.cache_dir.join(cache_name(name));
        if !self.store.fetch(name, &dest)? {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&dest).map_err(EngineError::io_at(&dest))?;
        Ok(Some(text.trim().to_string()))
    }

    /// Fetches one index blob into the cache and keeps it only if it was
    /// built with this configuration. An incompatible blob is logged and
    /// skipped, so that a store carrying an older lineage neither wins over a
    /// compatible one nor stops the ancestor walk.
    fn fetch_if_compatible(&self, name: &str) -> Result<Option<PathBuf>> {
        let dest = self.cache_dir.join(cache_name(name));
        if !self.store.fetch(name, &dest)? {
            return Ok(None);
        }
        // A blob written by an older build of cambium is not corrupt, just
        // stale: a store outlives format bumps the same way it outlives config
        // edits, and the walk must step past it rather than stop. A blob that
        // is not a readable file at all — a zstd frame that will not
        // decompress, a truncated transfer some other tool left behind — is
        // skipped for the same reason: it is one key in a walk over many, and
        // the next one may be fine.
        let f = match IndexFile::open(&dest) {
            Ok(f) => f,
            Err(
                e @ (cambium_index::IndexError::BadVersion { .. }
                | cambium_index::IndexError::BadMagic { .. }
                | cambium_index::IndexError::Decompress { .. }),
            ) => {
                tracing::warn!(key = name, error = %e, "skipping an index this build cannot read");
                let _ = std::fs::remove_file(&dest);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if f.view().config_hash != self.config_hash {
            tracing::warn!(
                key = name,
                "skipping an index built with a different cambium.toml (or an older rule-hash \
                 version or index content revision)"
            );
            return Ok(None);
        }
        Ok(Some(dest))
    }

    /// Publishes an index under `indexes/<lineage>/<tree>.bin`, with a
    /// `commits/<lineage>/<sha>` pointer naming the tree, plus lineage-less
    /// copies of both when the store cannot be listed. Returns every key
    /// written.
    ///
    /// Idempotent: the payload is a function of the graph, so republishing the
    /// same tree rewrites identical bytes and concurrent publishers cannot
    /// disagree. Two commits with the same tree publish to the same key and
    /// differ only in the provenance trailer, which records whichever commit
    /// wrote last.
    pub fn publish(
        &self,
        path: &Path,
        lineage: &[u8; 16],
        commit: &str,
        tree: &str,
    ) -> Result<Vec<String>> {
        let listable = self.store.list().is_ok();
        let mut names = vec![format::index_key(lineage, tree)];
        if !listable {
            names.push(tree_addressed_key(tree));
        }
        for n in &names {
            self.store.publish(n, path)?;
        }
        if tree != commit {
            std::fs::create_dir_all(self.cache_dir).map_err(EngineError::io_at(self.cache_dir))?;
            let pointer_file = self
                .cache_dir
                .join(format!("pointer.{}", format::temp_suffix()));
            std::fs::write(&pointer_file, format!("{tree}\n"))
                .map_err(EngineError::io_at(&pointer_file))?;
            let mut pointers = vec![format::commit_pointer_key(lineage, commit)];
            if !listable {
                pointers.push(commit_addressed_pointer(commit));
            }
            let published = pointers
                .iter()
                .try_for_each(|p| self.store.publish(p, &pointer_file));
            let _ = std::fs::remove_file(&pointer_file);
            published?;
            names.extend(pointers);
        }
        Ok(names)
    }
}

/// Fallback key for stores that cannot be listed: no lineage in the path,
/// because a fetcher has no way to learn it first.
pub fn tree_addressed_key(tree: &str) -> String {
    format!("indexes/by-tree/{tree}.bin")
}

/// The sha-addressed pointer for unlistable stores.
pub fn commit_addressed_pointer(commit: &str) -> String {
    format!("commits/by-commit/{commit}")
}

/// The key the previous, sha-addressed layout wrote for unlistable stores.
/// Still read, never written.
pub fn commit_addressed_key(commit: &str) -> String {
    format!("indexes/by-commit/{commit}.bin")
}

/// Local cache file name for a store key.
pub fn cache_name(key: &str) -> String {
    key.replace('/', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{DirStore, StoreError};
    use cambium_index::{IndexBuilder, IndexData, PackageUpdate, TargetRow};

    const HASH: [u8; 32] = [9; 32];

    fn index(commit: &str, lineage: [u8; 16], config_hash: [u8; 32]) -> IndexData {
        let mut b = IndexBuilder::new(lineage);
        b.set_config_hash(config_hash);
        b.apply_packages(&[PackageUpdate {
            package: "lib".into(),
            build_hash: [1; 32],
            bzl_hash: [0; 32],
            targets: vec![TargetRow::rule("//lib:core", "cc_library", vec![])],
        }]);
        b.finish(commit)
    }

    fn write(dir: &Path, idx: &IndexData) -> PathBuf {
        let p = dir.join(format!("{}.bin", idx.provenance.commit));
        format::write_index(&p, idx).unwrap();
        p
    }

    /// A [`DirStore`] that pretends it cannot enumerate, the way plain HTTP
    /// cannot.
    struct Unlistable(DirStore);

    impl ArtifactStore for Unlistable {
        fn fetch(&self, name: &str, dest: &Path) -> std::result::Result<bool, StoreError> {
            self.0.fetch(name, dest)
        }
        fn publish(&self, name: &str, src: &Path) -> std::result::Result<(), StoreError> {
            self.0.publish(name, src)
        }
        fn list(&self) -> std::result::Result<Vec<String>, StoreError> {
            Err(StoreError::Unsupported("list"))
        }
        fn describe(&self) -> String {
            "unlistable".into()
        }
    }

    #[test]
    fn a_listable_store_gets_lineage_keys_only_and_is_found_by_tree_or_sha() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore::new(dir.path().join("store"));
        let cache = dir.path().join("cache");
        let c = BaselineCache::new(&store, &cache, HASH);
        let idx = index("sha1", [2; 16], HASH);
        let path = write(dir.path(), &idx);

        let names = c.publish(&path, &[2; 16], "sha1", "tree1").unwrap();
        assert_eq!(
            names,
            vec![
                format::index_key(&[2; 16], "tree1"),
                format::commit_pointer_key(&[2; 16], "sha1")
            ]
        );
        assert!(c.fetch("sha1", "tree1").unwrap().is_some(), "by tree");
        assert!(
            c.fetch("sha1", "sha1").unwrap().is_some(),
            "by sha alone, through the commits/ pointer"
        );
        assert!(c.fetch("other", "other").unwrap().is_none());
    }

    #[test]
    fn an_unlistable_store_gets_the_lineage_less_copies_and_reads_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Unlistable(DirStore::new(dir.path().join("store")));
        let cache = dir.path().join("cache");
        let c = BaselineCache::new(&store, &cache, HASH);
        let idx = index("sha1", [2; 16], HASH);
        let path = write(dir.path(), &idx);

        let names = c.publish(&path, &[2; 16], "sha1", "tree1").unwrap();
        assert!(names.contains(&tree_addressed_key("tree1")), "{names:?}");
        assert!(
            names.contains(&commit_addressed_pointer("sha1")),
            "{names:?}"
        );
        assert!(c.fetch("sha1", "tree1").unwrap().is_some());
        assert!(
            c.fetch("sha1", "sha1").unwrap().is_some(),
            "via the pointer"
        );
    }

    #[test]
    fn an_index_from_another_configuration_is_not_a_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore::new(dir.path().join("store"));
        let cache = dir.path().join("cache");
        let c = BaselineCache::new(&store, &cache, HASH);
        let stale = index("sha1", [1; 16], [0; 32]);
        c.publish(&write(dir.path(), &stale), &[1; 16], "sha1", "tree1")
            .unwrap();
        assert!(c.fetch("sha1", "tree1").unwrap().is_none());

        // A compatible one in another lineage wins, wherever it sorts.
        let good = index("sha1", [3; 16], HASH);
        c.publish(&write(dir.path(), &good), &[3; 16], "sha1", "tree1")
            .unwrap();
        let got = c.fetch("sha1", "tree1").unwrap().expect("found");
        assert_eq!(IndexFile::open(&got).unwrap().view().lineage, [3; 16]);
    }

    #[test]
    fn cache_names_are_flat() {
        assert_eq!(
            cache_name("indexes/ab/cd.bin"),
            "indexes_ab_cd.bin",
            "one file per key, no directories to create"
        );
    }
}

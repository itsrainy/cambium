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
    diags: crate::diag::Diagnostics,
    /// Lineages (hex) of the indexes a fetch refused for their config hash,
    /// so the lineage probe can name them without reading the W003 text.
    refused: std::cell::RefCell<Vec<String>>,
    /// Write the lineage-less keys on publish even to a store that can list
    /// (`baseline.lineage_less_copies`).
    lineage_less_copies: bool,
}

impl<'a> BaselineCache<'a> {
    pub fn new(store: &'a dyn ArtifactStore, cache_dir: &'a Path, config_hash: [u8; 32]) -> Self {
        Self {
            store,
            cache_dir,
            config_hash,
            diags: crate::diag::Diagnostics::new(),
            refused: std::cell::RefCell::new(Vec::new()),
            lineage_less_copies: false,
        }
    }

    /// Also writes `indexes/by-tree/<tree>.bin` and
    /// `commits/by-commit/<sha>` when the store can list, for readers of the
    /// same bytes that cannot (`baseline.lineage_less_copies`).
    pub fn with_lineage_less_copies(mut self, on: bool) -> Self {
        self.lineage_less_copies = on;
        self
    }

    /// Reports into the engine's sink rather than into a buffer nobody reads.
    pub fn with_diagnostics(mut self, diags: crate::diag::Diagnostics) -> Self {
        self.diags = diags;
        self
    }

    /// The lineages of every index a fetch so far refused because it was
    /// built with another configuration, sorted and each once.
    pub fn refused_lineages(&self) -> Vec<String> {
        let mut v = self.refused.borrow().clone();
        v.sort();
        v.dedup();
        v
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
        // A backend that cannot enumerate falls back to the blind keys below.
        // An io failure is not that: it is a store that is there and cannot
        // be read (a directory store whose root is a file, or refused), which
        // is exit 5, not a walk over keys that cannot be there either.
        let listing = match self.store.list() {
            Ok(names) => Some(names),
            Err(e @ crate::store::StoreError::Io { .. }) => return Err(e.into()),
            Err(_) => None,
        };
        if let Some(names) = listing {
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
            self.diags.emit(
                crate::diag::Diagnostic::warning(
                    crate::diag::Code::W003IndexConfigMismatch,
                    format!(
                        "skipping the stored index `{name}`: it was built with a different \
                         cambium.toml, an older rule-hash version or an older index content \
                         revision, so splicing onto it could disagree with a full build"
                    ),
                )
                .with_help(
                    "expected right after a cambium.toml edit; mint a new lineage with \
                     `cambi index build --publish`",
                ),
            );
            self.refused
                .borrow_mut()
                .push(crate::inspect::hex(&f.view().lineage));
            return Ok(None);
        }
        Ok(Some(dest))
    }

    /// Publishes an index under `indexes/<lineage>/<tree>.bin`, with a
    /// `commits/<lineage>/<sha>` pointer naming the tree, plus lineage-less
    /// copies of both when the store cannot be listed or
    /// [`with_lineage_less_copies`](Self::with_lineage_less_copies) asks for
    /// them. Returns every key written.
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
        // The lineage-less copies go wherever a reader may not be able to
        // list: always when this store cannot, and on request when it can but
        // a reader in front of the same bytes cannot (plain HTTP over the
        // bucket an `aws s3` publisher writes).
        let lineage_less = self.lineage_less_copies || !self.store.listable();
        let mut names = vec![format::index_key(lineage, tree)];
        if lineage_less {
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
            if lineage_less {
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

/// A lineage in the store that this `cambi` cannot splice onto, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleLineage {
    /// The lineage, as hex.
    pub lineage: String,
    pub why: Stale,
}

/// Why a lineage is stale. Both are the refusals a fetch makes (`CAMBI-W018`
/// and `CAMBI-W003`/`CAMBI-W019`), decided once per lineage from one of its
/// indexes: every index of a lineage shares its configuration and format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stale {
    /// Written in an older on-disk format than this `cambi` reads.
    OlderFormat { found: u32, expected: u32 },
    /// Built with a different `cambium.toml`, rule-hash version or index
    /// content revision.
    Config,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stale::OlderFormat { found, expected } => {
                write!(f, "format version {found}; this cambi reads {expected}")
            }
            Stale::Config => f.write_str(
                "built with a different cambium.toml, rule-hash version or index content \
                 revision",
            ),
        }
    }
}

/// What garbage collection removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    /// Each stale lineage removed, with how many of its keys went.
    pub stale: Vec<(StaleLineage, usize)>,
    /// Keys removed for being older than `retention_days`.
    pub aged: usize,
}

impl Collected {
    /// Every key removed.
    pub fn total(&self) -> usize {
        self.aged + self.stale.iter().map(|(_, n)| n).sum::<usize>()
    }
}

impl BaselineCache<'_> {
    /// The lineages in the store a fetch would refuse: those whose indexes are
    /// in an older format than this `cambi` reads, or were built with another
    /// configuration. A lineage is judged by the first of its indexes that
    /// opens or is refused for its format; one whose files are all unreadable
    /// for any other reason (a truncated transfer) is left to the age rule,
    /// and so is a lineage in a *newer* format, which is a newer `cambi`'s to
    /// judge.
    pub fn stale_lineages(&self) -> Result<Vec<StaleLineage>> {
        let names = self.store.list()?;
        let mut by_lineage: std::collections::BTreeMap<String, Vec<&str>> = Default::default();
        for n in &names {
            if let Some((lineage, _)) = format::parse_index_key(n) {
                if is_lineage(&lineage) {
                    by_lineage.entry(lineage).or_default().push(n);
                }
            }
        }
        let mut stale = Vec::new();
        for (lineage, keys) in by_lineage {
            for key in keys {
                match self.judge(key)? {
                    Judged::Live => break,
                    Judged::Stale(why) => {
                        stale.push(StaleLineage { lineage, why });
                        break;
                    }
                    Judged::Unknown => continue,
                }
            }
        }
        Ok(stale)
    }

    /// Reads one stored index's header: in place on a directory store,
    /// through the cache (and deleted again) on any other.
    fn judge(&self, key: &str) -> Result<Judged> {
        let (path, scratch) = match self.store.local_path(key) {
            Some(p) => (p, false),
            None => {
                std::fs::create_dir_all(self.cache_dir)
                    .map_err(EngineError::io_at(self.cache_dir))?;
                let dest = self.cache_dir.join(format!(
                    "{}.gc.{}",
                    cache_name(key),
                    format::temp_suffix()
                ));
                if !self.store.fetch(key, &dest)? {
                    return Ok(Judged::Unknown);
                }
                (dest, true)
            }
        };
        let judged = match IndexFile::open(&path) {
            Ok(f) if f.view().config_hash == self.config_hash => Judged::Live,
            Ok(_) => Judged::Stale(Stale::Config),
            Err(cambium_index::IndexError::BadVersion {
                found, expected, ..
            }) if found < expected => Judged::Stale(Stale::OlderFormat { found, expected }),
            Err(e) => {
                tracing::debug!(key, error = %e, "cannot judge this index's lineage from it");
                Judged::Unknown
            }
        };
        if scratch {
            let _ = std::fs::remove_file(&path);
        }
        Ok(judged)
    }

    /// `cambi index gc`: removes every key of every stale lineage (unless
    /// `keep_incompatible`), then every key older than `retention_days`.
    /// `keep` survives both.
    ///
    /// A backend that cannot collect is refused before anything is read, with
    /// the same `CAMBI-E012` the age rule alone always gave it.
    pub fn collect(
        &self,
        retention_days: u64,
        keep: &str,
        keep_incompatible: bool,
    ) -> Result<Collected> {
        if !self.store.collects_garbage() {
            return Err(crate::store::StoreError::Unsupported("gc").into());
        }
        let mut out = Collected::default();
        if !keep_incompatible {
            let stale = self.stale_lineages()?;
            if !stale.is_empty() {
                let names = self.store.list()?;
                for s in stale {
                    let mut removed = 0;
                    for n in names
                        .iter()
                        .filter(|n| n.as_str() != keep && in_lineage(n, &s.lineage))
                    {
                        if self.store.remove(n)? {
                            removed += 1;
                        }
                    }
                    out.stale.push((s, removed));
                }
            }
        }
        out.aged = self.store.gc(retention_days, keep)?;
        Ok(out)
    }
}

/// What one index says about its lineage.
enum Judged {
    Live,
    Stale(Stale),
    /// Nothing, from this file: try another.
    Unknown,
}

/// A lineage id as keys spell it: 32 hex digits. `by-tree` and `by-commit`
/// hold the lineage-less copies and are nobody's lineage.
fn is_lineage(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `key` is one of `lineage`'s index files or `commits/` pointers.
fn in_lineage(key: &str, lineage: &str) -> bool {
    format::parse_index_key(key).is_some_and(|(l, _)| l == lineage)
        || format::parse_commit_pointer_key(key).is_some_and(|(l, _)| l == lineage)
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
    fn a_listable_publisher_writes_lineage_less_copies_when_asked_so_a_blind_reader_finds_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let idx = index("sha1", [2; 16], HASH);
        let path = write(dir.path(), &idx);

        // Off: the listable publisher writes lineage keys only, and a reader
        // that cannot list (plain HTTP over the same bytes) finds nothing.
        let writer = DirStore::new(root.clone());
        BaselineCache::new(&writer, &dir.path().join("w"), HASH)
            .publish(&path, &[2; 16], "sha1", "tree1")
            .unwrap();
        let reader = Unlistable(DirStore::new(root.clone()));
        let r1 = dir.path().join("r1");
        let blind = BaselineCache::new(&reader, &r1, HASH);
        assert!(blind.fetch("sha1", "tree1").unwrap().is_none());

        // On: the same publish also writes the by-tree and by-commit keys.
        let names = BaselineCache::new(&writer, &dir.path().join("w"), HASH)
            .with_lineage_less_copies(true)
            .publish(&path, &[2; 16], "sha1", "tree1")
            .unwrap();
        assert!(names.contains(&tree_addressed_key("tree1")), "{names:?}");
        assert!(
            names.contains(&commit_addressed_pointer("sha1")),
            "{names:?}"
        );
        let r2 = dir.path().join("r2");
        let blind = BaselineCache::new(&reader, &r2, HASH);
        assert!(blind.fetch("sha1", "tree1").unwrap().is_some());
        assert!(
            blind.fetch("sha1", "sha1").unwrap().is_some(),
            "via the pointer"
        );
    }

    #[test]
    fn a_directory_store_that_cannot_be_read_fails_the_lookup_with_exit_5() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"").unwrap();
        let store = DirStore::new(file.join("nope"));
        let cache = dir.path().join("cache");
        let c = BaselineCache::new(&store, &cache, HASH);
        let err = c.fetch("sha1", "tree1").unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err}");
        assert_eq!(err.diagnostic().code, crate::diag::Code::E011StoreIo);
    }

    #[test]
    fn a_directory_store_that_does_not_exist_is_a_miss_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let diags = crate::diag::Diagnostics::new();
        let store = DirStore::new(dir.path().join("none")).with_diagnostics(diags.clone());
        let cache = dir.path().join("cache");
        let c = BaselineCache::new(&store, &cache, HASH);
        assert!(c.fetch("sha1", "tree1").unwrap().is_none());
        assert!(c.fetch("sha0", "tree0").unwrap().is_none());
        let said: Vec<_> = diags.drain().iter().map(|d| d.code).collect();
        assert_eq!(said, vec![crate::diag::Code::W007StoreFetchFailed]);
    }

    /// Publishes `idx` raw (uncompressed), with its format version set to
    /// `version`, the way an older or newer `cambi` would have left it.
    fn publish_as_version(c: &BaselineCache<'_>, dir: &Path, idx: &IndexData, version: u32) {
        let p = dir.join(format!("{}.v{version}.bin", idx.provenance.commit));
        format::write_index_with(&p, idx, format::Compression::None).unwrap();
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[8..12].copy_from_slice(&version.to_le_bytes());
        std::fs::write(&p, bytes).unwrap();
        c.publish(&p, &idx.header.lineage, &idx.provenance.commit, "tree1")
            .unwrap();
    }

    #[test]
    fn gc_drops_the_lineages_this_cambi_cannot_read_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let store = DirStore::new(root.clone());
        let cache = dir.path().join("cache");
        let c = BaselineCache::new(&store, &cache, HASH);
        // Live: this configuration, this format.
        let live = index("sha1", [1; 16], HASH);
        c.publish(&write(dir.path(), &live), &[1; 16], "sha1", "tree1")
            .unwrap();
        // Another configuration.
        let other = index("sha2", [2; 16], [0; 32]);
        c.publish(&write(dir.path(), &other), &[2; 16], "sha2", "tree1")
            .unwrap();
        // An older format, and a newer one, which is not this cambi's to judge.
        let v = format::FORMAT_VERSION;
        publish_as_version(&c, dir.path(), &index("sha3", [3; 16], HASH), v - 1);
        publish_as_version(&c, dir.path(), &index("sha4", [4; 16], HASH), v + 1);

        let hex = |b: u8| crate::inspect::hex(&[b; 16]);
        let stale = c.stale_lineages().unwrap();
        assert_eq!(
            stale,
            vec![
                StaleLineage {
                    lineage: hex(2),
                    why: Stale::Config
                },
                StaleLineage {
                    lineage: hex(3),
                    why: Stale::OlderFormat {
                        found: v - 1,
                        expected: v
                    }
                },
            ]
        );

        // `--keep-incompatible`: by age alone, and nothing is that old.
        let got = c.collect(30, "", true).unwrap();
        assert_eq!(got, Collected::default());
        assert_eq!(store.list().unwrap().len(), 8);

        // A pinned key of a stale lineage survives; the rest of it goes.
        let pinned = format::commit_pointer_key(&[2; 16], "sha2");
        let got = c.collect(30, &pinned, false).unwrap();
        assert_eq!(got.total(), 3, "{got:?}");
        assert_eq!(got.stale.len(), 2);
        let mut left = store.list().unwrap();
        left.sort();
        let mut want = vec![
            format::index_key(&[1; 16], "tree1"),
            format::commit_pointer_key(&[1; 16], "sha1"),
            pinned,
            format::index_key(&[4; 16], "tree1"),
            format::commit_pointer_key(&[4; 16], "sha4"),
        ];
        want.sort();
        assert_eq!(left, want);
        assert!(
            !root.join("indexes").join(hex(3)).exists(),
            "an emptied lineage directory goes with its last key"
        );
    }

    #[test]
    fn gc_on_a_store_that_cannot_collect_reads_nothing_and_says_e012() {
        let dir = tempfile::tempdir().unwrap();
        let store = Unlistable(DirStore::new(dir.path().join("store")));
        let cache = dir.path().join("cache");
        let err = BaselineCache::new(&store, &cache, HASH)
            .collect(30, "", false)
            .unwrap_err();
        assert_eq!(
            err.diagnostic().code,
            crate::diag::Code::E012StoreUnsupported
        );
        assert!(!cache.exists(), "nothing was fetched to find that out");
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

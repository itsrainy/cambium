//! Shared fixtures for the integration tests: an in-memory checkout, a
//! checkout on disk, the repository mapping the corpus probes read from the
//! environment, and the two-line derivation of a checkout's repo inputs.
//!
//! Each test binary compiles this module on its own, so an item one binary
//! does not use is not dead code in any useful sense.
#![allow(dead_code)]

use std::collections::BTreeSet;

use cambium_index::externals::{build_repo_inputs, read_live_module_facts, RepoInputs};
use cambium_index::{FileText, IndexFile, LoadGraph, RepoMapping};

/// A tiny in-memory checkout: repo-relative path to contents.
pub struct Files(pub Vec<(&'static str, &'static str)>);

impl FileText for Files {
    fn text(&self, path: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, t)| t.to_string())
    }
}

/// A working tree on disk, for the corpus probes.
pub struct Dir(pub std::path::PathBuf);

impl FileText for Dir {
    fn text(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(path)).ok()
    }
}

/// The mapping envoy's own `MODULE.bazel` implies: `envoy` is the main
/// repository and `envoy_api` a bzlmod module.
pub fn envoy_mapping() -> RepoMapping {
    RepoMapping::from_dump("{\"\":\"\",\"envoy\":\"\",\"envoy_api\":\"envoy_api+\"}")
}

/// The repo inputs of a checkout, derived the way the engine derives them.
pub fn inputs(files: &dyn FileText, m: &RepoMapping) -> RepoInputs {
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(files, m);
    let facts = read_live_module_facts(files, m);
    build_repo_inputs(files, m, &g, &ws, &facts)
}

pub fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// The repository mapping a corpus probe runs under, read from the
/// environment:
///
/// - `CAMBIUM_CORPUS_ALIAS` — the main repository's own apparent name
///   (default `envoy`), added as a main alias.
/// - `CAMBIUM_CORPUS_INDEX=<file>` — a stored index whose canonical column
///   supplies the mapping, the way a replay's `--rederive` rebuilds it
///   without `bazel mod dump_repo_mapping`. Absent, the identity mapping.
/// - `CAMBIUM_CORPUS_MAP_PICK=<substring>` — keep only the repositories whose
///   apparent name contains it.
/// - `CAMBIUM_CORPUS_MAP_LIMIT=<n>` — keep at most that many.
pub fn corpus_mapping() -> RepoMapping {
    let alias = std::env::var("CAMBIUM_CORPUS_ALIAS").unwrap_or_else(|_| "envoy".into());
    match std::env::var("CAMBIUM_CORPUS_INDEX") {
        Ok(f) => {
            let file = IndexFile::open(std::path::Path::new(&f)).expect("index file");
            let v = file.view();
            let limit: usize = std::env::var("CAMBIUM_CORPUS_MAP_LIMIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(usize::MAX);
            let pick = std::env::var("CAMBIUM_CORPUS_MAP_PICK").unwrap_or_default();
            RepoMapping::from_canonical(
                (0..v.num_repos())
                    .filter(|&r| pick.is_empty() || v.repo_name(r).contains(pick.as_str()))
                    .take(limit)
                    .map(|r| (v.repo_name(r), v.repo_canonical.get(r))),
            )
        }
        Err(_) => RepoMapping::identity(),
    }
    .with_main_alias(&alias)
}

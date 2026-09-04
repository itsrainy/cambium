//! A [`PackageQuerier`] backed by a synthetic graph.
//!
//! This is what makes spec 12.4 possible: the real engine — dirty sets,
//! splices, direct-set derivation, the walk — driven over graphs of arbitrary
//! size with no Bazel process anywhere, and checked against the naive reference
//! algorithm.

use cambium_index::PackageUpdate;
use cambium_synth::SynthGraph;

use crate::error::EngineError;
use crate::query::PackageQuerier;

pub struct SynthQuerier<'a> {
    pub graph: &'a SynthGraph,
}

impl<'a> SynthQuerier<'a> {
    pub fn new(graph: &'a SynthGraph) -> Self {
        Self { graph }
    }
}

impl PackageQuerier for SynthQuerier<'_> {
    fn query(&self, packages: &[String]) -> Result<Vec<PackageUpdate>, EngineError> {
        Ok(packages
            .iter()
            .map(|p| match self.graph.pkg_by_path.get(p) {
                Some(&id) if self.graph.packages[id as usize].alive => {
                    self.graph.package_update(id)
                }
                // A package that no longer exists still needs an (empty) update
                // so that its old rows are tombstoned, exactly as Bazel
                // returning nothing for it would.
                _ => PackageUpdate {
                    package: p.clone(),
                    ..Default::default()
                },
            })
            .collect())
    }

    fn universe(&self) -> Result<Vec<PackageUpdate>, EngineError> {
        Ok(self.graph.package_updates())
    }

    fn rbuildfiles(&self, bzl_paths: &[String]) -> Result<Vec<String>, EngineError> {
        let mut out: Vec<String> = Vec::new();
        for path in bzl_paths {
            let Some(b) = self.graph.bzl.iter().find(|b| &b.path == path) else {
                continue;
            };
            for &p in &b.loaded_by {
                if self.graph.packages[p as usize].alive {
                    out.push(self.graph.packages[p as usize].path.clone());
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn version(&self) -> String {
        "synth".to_string()
    }
}

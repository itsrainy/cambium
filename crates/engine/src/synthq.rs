//! A [`PackageQuerier`] backed by a synthetic graph.
//!
//! This is what makes spec 12.4 possible: the real engine — dirty sets,
//! splices, direct-set derivation, the walk — driven over graphs of arbitrary
//! size with no Bazel process anywhere, and checked against the naive reference
//! algorithm.

use cambium_index::PackageUpdate;
use cambium_synth::{SynthFiles, SynthGraph};

use crate::error::EngineError;
use crate::query::{PackageQuerier, Rows};

pub struct SynthQuerier<'a> {
    pub graph: &'a SynthGraph,
    files: SynthFiles<'a>,
}

impl<'a> SynthQuerier<'a> {
    pub fn new(graph: &'a SynthGraph) -> Self {
        Self {
            graph,
            files: SynthFiles(graph),
        }
    }
}

impl PackageQuerier for SynthQuerier<'_> {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
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
            .collect::<Vec<_>>()
            .into())
    }

    fn universe(&self) -> Result<Rows, EngineError> {
        Ok(self.graph.package_updates().into())
    }

    /// The synthetic workspace's own BUILD and `.bzl` text, so the static
    /// load closure is computed by the same code a real run uses.
    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        Some(&self.files)
    }

    /// The same set of files [`SynthFiles`] can serve text for: the witness
    /// enumerates the synthetic tree instead of a directory.
    fn starlark_files(&self) -> Option<Vec<String>> {
        let g = self.graph;
        let mut out = vec!["MODULE.bazel".to_string(), "rules/synth.bzl".to_string()];
        out.extend(g.bzl_by_path.keys().cloned());
        for p in &g.packages {
            if !p.alive {
                continue;
            }
            out.push(if p.path.is_empty() {
                "BUILD.bazel".to_string()
            } else {
                format!("{}/BUILD.bazel", p.path)
            });
        }
        out.sort();
        out.dedup();
        Some(out)
    }

    fn version(&self) -> String {
        "synth".to_string()
    }
}

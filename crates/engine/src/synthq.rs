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
    /// Paths in the tree that no target names: a README, a directory of
    /// notes. A cold `//...` walk lists every directory and a refresh only
    /// what re-querying its packages reached, which is the difference the
    /// bench's rebuild oracle classifies, so a test can hand a querier the
    /// loose paths one side of it saw.
    loose: Vec<String>,
}

impl<'a> SynthQuerier<'a> {
    pub fn new(graph: &'a SynthGraph) -> Self {
        Self::with_loose_paths(graph, Vec::new())
    }

    /// A querier whose tree also holds `loose`, paths no target names.
    pub fn with_loose_paths(graph: &'a SynthGraph, loose: Vec<String>) -> Self {
        Self {
            graph,
            files: SynthFiles(graph),
            loose,
        }
    }
}

impl SynthQuerier<'_> {
    /// Every path of the synthetic workspace: the Starlark files, the source
    /// files each live target names, and the loose paths. A `glob` sees the
    /// second kind, so the loading layer needs them to have directory
    /// listings.
    fn tree_paths(&self) -> Vec<String> {
        let mut out = self.starlark_files().unwrap_or_default();
        out.extend(self.loose.iter().cloned());
        for p in &self.graph.packages {
            if !p.alive {
                continue;
            }
            for &tid in &p.targets {
                let t = &self.graph.targets[tid as usize];
                for &slot in &t.srcs {
                    out.push(self.graph.src_path(tid, slot));
                }
            }
        }
        out.sort();
        out.dedup();
        out
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

    /// The loading layer a Bazel run over this synthetic workspace would have
    /// left behind (`cambium_synth::sky`). The synthetic world is a model
    /// either way; what matters is that the engine reads it the same way it
    /// reads a real dump, through the same parser and the same walk.
    fn can_dump_loading_layer(&self) -> bool {
        true
    }

    fn dump_loading_layer(
        &self,
        _filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), EngineError> {
        let packages: Vec<String> = self
            .graph
            .packages
            .iter()
            .filter(|p| p.alive)
            .map(|p| p.path.clone())
            .collect();
        let tree = cambium_synth::sky::Listed {
            paths: self.tree_paths(),
            text: &self.files,
        };
        Ok((
            cambium_synth::sky::dump(&packages, &tree),
            cambium_synth::sky::paths(),
        ))
    }

    fn version(&self) -> String {
        "synth".to_string()
    }
}

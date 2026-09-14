//! The environment a result was produced in, stamped into every walk.
//!
//! The old results recorded the Bazel version and the cambium version and
//! nothing else, so a 165-target difference between two machines on one pair
//! had no field to attach itself to. Everything a reader needs to know
//! whether two numbers are comparable lives here, and it travels in the
//! result: the machine, the OS, the Bazel that answered, the cambium that was
//! measured, the harness's own sha, the engine identity and configuration
//! hash, and where each tool wrote so no two tools shared an output base.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::harness::Machine;

/// What a walk was run in. Stamped into [`super::walk::model::WalkResult`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Environment {
    /// CPU, cores, RAM, OS+arch — enough to refuse a cross-machine
    /// comparison.
    pub machine: Machine,
    /// `bazel --version` as it actually answered, e.g. `bazel 7.4.1`. Empty
    /// when the binary could not be asked.
    pub bazel_version_used: String,
    /// `cambi --version`: the version and short sha of the measured binary.
    pub cambi_version: String,
    /// `crate::util::tool_sha`: the harness's own build.
    pub tool_sha: String,
    /// `State::engine_id`: the index format plus the engine configuration, so
    /// a resume under a different engine starts over.
    pub engine_id: String,
    /// The subject's effective configuration hash, hex.
    pub config_hash: String,
    /// Where each tool wrote, so a reader can see they did not share a Bazel
    /// output base. Keyed by tool name; cambium's is its scratch head-index
    /// directory, a peer's is its own worktree.
    #[serde(default)]
    pub output_bases: BTreeMap<String, String>,
}

impl Environment {
    /// Whether two environments are close enough that their walls may be read
    /// against each other: the same machine and the same Bazel. A different
    /// cambium or harness sha is the whole point of a comparison, so those do
    /// not gate it.
    pub fn comparable_to(&self, other: &Environment) -> bool {
        self.machine.fingerprint() == other.machine.fingerprint()
            && self.bazel_version_used == other.bazel_version_used
    }
}

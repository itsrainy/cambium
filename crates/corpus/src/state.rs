//! The resumable per-repo state file, `corpus-results/<id>/state.json`.
//!
//! Every subcommand records what it finished, so a killed TensorFlow run does
//! not restart the external fetch, and `corpus all` can pick up where the last
//! invocation stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATE_FILE: &str = "state.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub schema: u32,
    pub id: String,
    pub pin: String,
    /// The mirror and worktree exist at `pin`.
    #[serde(default)]
    pub fetched: bool,
    /// Cold builds that completed, by commit, with the index file each wrote.
    #[serde(default)]
    pub built: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub replay: Option<ReplayState>,
    /// Pair keys (`<base>..<head>`) the comparison has finished.
    #[serde(default)]
    pub compared: BTreeSet<String>,
    #[serde(default)]
    pub reported: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ReplayState {
    pub from: String,
    pub to: String,
    /// Bazel version string the replay ran under.
    #[serde(default)]
    pub bazel_version: String,
    /// The newest commit whose index was materialised and published.
    pub last_completed: Option<String>,
    pub done: bool,
}

impl State {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(STATE_FILE)
    }

    /// Loads the state for `id`, or a fresh one. A state file written for a
    /// different pin is discarded: results are per pin.
    pub fn load(dir: &Path, id: &str, pin: &str) -> anyhow::Result<Self> {
        let path = Self::path(dir);
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let st: State = serde_json::from_str(&s)
                    .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
                if st.id == id && st.pin == pin {
                    return Ok(st);
                }
                tracing::warn!(
                    path = %path.display(),
                    old_pin = %st.pin,
                    new_pin = %pin,
                    "state file is for a different pin; starting over"
                );
                Ok(Self::fresh(id, pin))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::fresh(id, pin)),
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        }
    }

    pub fn fresh(id: &str, pin: &str) -> Self {
        Self {
            schema: cambium_engine::report::SCHEMA_VERSION,
            id: id.to_string(),
            pin: pin.to_string(),
            ..Default::default()
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path(dir);
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn pair_key(base: &str, head: &str) -> String {
        format!("{base}..{head}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_resets_on_a_new_pin() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = State::fresh("bazel", "a");
        st.fetched = true;
        st.compared.insert(State::pair_key("x", "y"));
        st.save(dir.path()).unwrap();
        assert_eq!(State::load(dir.path(), "bazel", "a").unwrap(), st);
        let other = State::load(dir.path(), "bazel", "b").unwrap();
        assert!(!other.fetched, "a new pin starts from scratch");
        assert_eq!(other.pin, "b");
    }
}

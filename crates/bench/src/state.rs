//! The resumable per-repo state file, `bench-results/<id>/state.json`.
//!
//! Every subcommand records what it finished, so a killed TensorFlow run does
//! not restart the external fetch, and a rerun can pick up where the last
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
    pub history: Option<HistoryState>,
    /// ChangeSet keys (`<base>..<head>`) the comparison has finished.
    #[serde(default)]
    pub compared: BTreeSet<String>,
    #[serde(default)]
    pub reported: bool,
    /// Which cambium wrote the rest of this file: the index format version and
    /// the configuration hash. A result is only reusable by the build that
    /// made it, so a state written by another one is thrown away rather than
    /// resumed. Empty in files written before the field existed, which the
    /// next load treats as a mismatch.
    #[serde(default)]
    pub engine: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HistoryState {
    pub from: String,
    pub to: String,
    /// Bazel version string the history ran under.
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
            schema: crate::RESULT_SCHEMA,
            id: id.to_string(),
            pin: pin.to_string(),
            ..Default::default()
        }
    }

    /// The identity of the cambium that will use this state: index format plus
    /// configuration hash, the two things that decide whether an index, a
    /// history or a comparison written earlier is the same experiment.
    pub fn engine_id(config_hash: &[u8; 32]) -> String {
        let hex: String = config_hash.iter().map(|b| format!("{b:02x}")).collect();
        format!("f{}/{}", cambium_index::format::FORMAT_VERSION, &hex[..16])
    }

    /// Loads the state and, when a different cambium wrote it, starts over:
    /// built indexes, the history and the compared set all belong to that
    /// other build. Resuming any of them would mix two experiments.
    pub fn load_for(dir: &Path, id: &str, pin: &str, engine_id: &str) -> anyhow::Result<Self> {
        let mut st = Self::load(dir, id, pin)?;
        if st.engine != engine_id {
            if !st.engine.is_empty() || !st.built.is_empty() || st.history.is_some() {
                tracing::warn!(
                    was = %st.engine,
                    now = %engine_id,
                    "results were written by a different cambium; starting over"
                );
            }
            st = Self {
                id: id.to_string(),
                pin: pin.to_string(),
                fetched: st.fetched,
                engine: engine_id.to_string(),
                ..Self::default()
            };
        }
        Ok(st)
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path(dir);
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn change_set_key(base: &str, head: &str) -> String {
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
        st.compared.insert(State::change_set_key("x", "y"));
        st.save(dir.path()).unwrap();
        assert_eq!(State::load(dir.path(), "bazel", "a").unwrap(), st);
        let other = State::load(dir.path(), "bazel", "b").unwrap();
        assert!(!other.fetched, "a new pin starts from scratch");
        assert_eq!(other.pin, "b");
    }
}

#[cfg(test)]
mod engine_identity {
    use super::*;

    #[test]
    fn a_state_from_another_cambium_starts_over_but_keeps_the_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = State {
            id: "r".into(),
            pin: "p".into(),
            fetched: true,
            engine: "f4/aaaa".into(),
            ..Default::default()
        };
        st.built.insert("p".into(), dir.path().join("index.bin"));
        st.compared.insert("a..b".into());
        st.save(dir.path()).unwrap();

        let same = State::load_for(dir.path(), "r", "p", "f4/aaaa").unwrap();
        assert_eq!(same.built.len(), 1);
        assert_eq!(same.compared.len(), 1);

        let other = State::load_for(dir.path(), "r", "p", "f5/aaaa").unwrap();
        assert!(other.fetched, "the mirror and worktree are still there");
        assert!(other.built.is_empty());
        assert!(other.compared.is_empty());
        assert!(other.history.is_none());
        assert_eq!(other.engine, "f5/aaaa");
    }

    #[test]
    fn the_id_changes_with_the_config_hash() {
        assert_ne!(State::engine_id(&[0; 32]), State::engine_id(&[1; 32]));
        assert!(State::engine_id(&[0; 32])
            .starts_with(&format!("f{}/", cambium_index::format::FORMAT_VERSION)));
    }
}

//! The resumable per-repo state file, `bench-results/<id>/state.json`.
//!
//! Every subcommand records what it finished, so a killed TensorFlow run does
//! not restart the external fetch, and a rerun can pick up where the last
//! invocation stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
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
    /// The walk's window and how far it got, under the name the history
    /// workload that preceded it gave the field.
    pub history: Option<HistoryState>,
    /// Change-set keys (`<base>..<head>`) the retired change-set comparison
    /// finished. Nothing adds to it since the walk replaced that workload; it
    /// stays so that `state.json` keeps its shape.
    #[serde(default)]
    pub compared: BTreeSet<String>,
    /// Which cambium wrote the rest of this file: the index format version and
    /// the configuration hash. A result is only reusable by the build that
    /// made it, so a state written by another one is thrown away rather than
    /// resumed. Empty in files written before the field existed, which the
    /// next load treats as a mismatch.
    #[serde(default)]
    pub engine: String,
    /// When each step started and last moved, RFC 3339, keyed
    /// `<step>.started`, `<step>.updated` and `<step>.done`.
    ///
    /// The state file said *what* had finished and never *when*, so elapsed
    /// and an ETA could not be computed from it and a watcher had nothing to
    /// show for a run whose progress file predates it. Absent from files
    /// written before the field existed, which reads as an empty table.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub steps: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HistoryState {
    pub from: String,
    pub to: String,
    /// Bazel version string the walk ran under.
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
                    .with_context(|| format!("parsing {}", path.display()))?;
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
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The state file as it stands, whatever subject or pin it is for, or
    /// `None` when there is none.
    ///
    /// For a *reader* (`cambium-bench watch`), which wants the file's own id,
    /// pin and step stamps and has no pin to check them against. [`load`] is
    /// for a run, and starting over on a pin mismatch (loudly) is the whole
    /// point of it there.
    ///
    /// [`load`]: State::load
    pub fn peek(dir: &Path) -> Option<Self> {
        let path = Self::path(dir);
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&text) {
            Ok(st) => Some(st),
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "unreadable state file");
                None
            }
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
        crate::util::write_json_atomic(&Self::path(dir), self)
    }

    /// Stamps `<step>.<event>` with the current time.
    pub fn stamp(&mut self, step: &str, event: &str) {
        self.steps
            .insert(format!("{step}.{event}"), crate::util::rfc3339_now());
    }

    /// Stamps `<step>.started` only the first time, so a resume keeps the
    /// clock the run actually started on.
    pub fn stamp_start(&mut self, step: &str) {
        self.steps
            .entry(format!("{step}.started"))
            .or_insert_with(crate::util::rfc3339_now);
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

    #[test]
    fn a_step_is_stamped_once_at_the_start_and_again_on_every_move() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = State::fresh("bazel", "a");
        st.stamp_start("history");
        let started = st.steps["history.started"].clone();
        st.stamp_start("history");
        assert_eq!(st.steps["history.started"], started, "a resume keeps it");
        st.stamp("history", "updated");
        st.stamp("history", "done");
        st.save(dir.path()).unwrap();
        let back = State::load(dir.path(), "bazel", "a").unwrap();
        assert_eq!(back.steps.len(), 3);
        assert!(crate::util::parse_rfc3339(&back.steps["history.done"]).is_some());
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

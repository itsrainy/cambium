//! Every document the harness writes to disk, in one place.
//!
//! Each has its own version constant, declared where its type lives; this
//! lists them so a reader can see the whole on-disk surface — and which
//! files a version bump orphans — without walking the modules. The
//! constants are re-exported here under one roof, not redefined.
//!
//! | file | schema | written by | read by |
//! |---|---|---|---|
//! | `bench-results/<subject>/state.json` | [`RESULT_SCHEMA`] | every workload | the next run, `watch` |
//! | `bench-results/<subject>/walk/walk-<pin>[-<ver>].json` | [`RESULT_SCHEMA`] | `walk` | `report`, `replay`, `watch` |
//! | `bench-results/<subject>/walk/labels/<base>..<head>/*.zst` | none | `walk` | `labels` |
//! | `bench-results/<subject>/progress.json` | [`PROGRESS_SCHEMA`] | every workload | `watch` |
//! | `bench-results/<subject>/index-<sha>[-<ver>].bin`, `bases/<sha>.bin`, `heads/<pin>.bin` | the index format | `subjects build`, `walk` | `replay` |
//! | `paths-<pin>[-<ver>].json` | [`REPLAY_SCHEMA`] | `replay --write-paths` | `replay` |
//! | a `--save-baseline` file | [`REPLAY_SCHEMA`] | `replay` | `replay --baseline` |
//! | the `watch --format json` snapshot | [`WATCH_SCHEMA`] | `watch` | anything |
//!
//! Only the result files are gated on read ([`crate::util::read_result`]):
//! an older result must be refused rather than half-read, where an older
//! progress file is a watcher's problem and degrades to "no progress".

pub use crate::progress::PROGRESS_SCHEMA;
pub use crate::replay::REPLAY_SCHEMA;
pub use crate::watch::WATCH_SCHEMA;
pub use crate::RESULT_SCHEMA;

/// The label-set files carry no schema: they are plain text, one label per
/// line, compressed. See [`crate::labels`].
pub const LABELS_SCHEMA: Option<u32> = None;

#[cfg(test)]
mod tests {
    /// The list above is the whole surface. A new constant elsewhere has to
    /// be added here, which is the point of the module.
    #[test]
    fn every_schema_constant_is_listed() {
        assert_eq!(super::RESULT_SCHEMA, crate::RESULT_SCHEMA);
        assert_eq!(super::PROGRESS_SCHEMA, crate::progress::PROGRESS_SCHEMA);
        assert_eq!(super::REPLAY_SCHEMA, crate::replay::REPLAY_SCHEMA);
        assert_eq!(super::WATCH_SCHEMA, crate::watch::WATCH_SCHEMA);
        assert!(super::LABELS_SCHEMA.is_none());
        let src = include_str!("lib.rs").to_string()
            + include_str!("progress.rs")
            + include_str!("replay.rs")
            + include_str!("watch.rs")
            + include_str!("harness.rs")
            + include_str!("labels.rs")
            + include_str!("state.rs")
            + include_str!("walk/model.rs")
            + include_str!("walk/driver.rs");
        let declared: Vec<&str> = src
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub const "))
            .filter(|l| l.contains("_SCHEMA:"))
            .filter_map(|l| l.split(':').next())
            .collect();
        for name in declared {
            assert!(
                include_str!("schema.rs").contains(name),
                "{name} is a schema constant this module does not list"
            );
        }
    }
}

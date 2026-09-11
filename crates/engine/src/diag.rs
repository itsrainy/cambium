//! Diagnostics: the warnings and errors a person or an agent has to act on.
//!
//! A diagnostic is not a log line. `tracing` says what the run did; a
//! diagnostic says what someone should do about it, and carries a stable code
//! so a script can branch on it and `docs/DIAGNOSTICS.md` can explain it. The
//! two live side by side: the `tracing::warn!` calls stay exactly where they
//! were, and a diagnostic is emitted beside the ones that are actionable.
//!
//! The rendering is the CLI's business (see `crates/cli/src/diag.rs`); this
//! module is the vocabulary and the sink.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// A diagnostic's stable identifier.
///
/// **The numbers are assigned once and never reused.** A code that is retired
/// keeps its variant and its number, and its row in `docs/DIAGNOSTICS.md`
/// says it is retired; a new diagnostic takes the next free number. Callers
/// grep for `CAMBI-W004` in stderr and in JSON, and a number that came to mean
/// something else would break them silently.
///
/// `Display` is the wire form (`CAMBI-W004`, `CAMBI-E009`) and is what both
/// the human rendering and the JSON carry.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    // ---- warnings ----
    /// A deprecated spelling was used and still worked: an old verb, an old
    /// flag, or an old value for one. Raised by the CLI's `compat` rewrite.
    W001DeprecatedSpelling,
    /// `cambium.toml` sets a key this cambium no longer honours.
    W002RemovedConfigKey,
    /// A stored index was skipped because it was built with a different
    /// `cambium.toml` (or an older rule-hash version).
    W003IndexConfigMismatch,
    /// A `.bazelrc` edit touched something that could move the graph, so the
    /// run is global.
    W004BazelrcGlobal,
    /// Dirty packages did not load under `--keep_going` and were tombstoned.
    W005PackagesTombstoned,
    /// `bazel query //...` finished with errors; the index is partial.
    W006PartialQuery,
    /// A store fetch command failed with something on stderr, and the
    /// artifact was read as absent.
    W007StoreFetchFailed,
    /// The base index carries no static `.bzl` load closure, so a changed
    /// `.bzl` cannot be resolved and the run is global.
    W008NoLoadClosure,
    /// A changed `.bzl` is loaded outside any package, so the run is global.
    W009WorkspaceLevelBzl,
    /// The witness found a changed `.bzl` that *is* loaded at head but has no
    /// node in the stored closure: the closure was incomplete.
    W010IncompleteClosure,
    /// The base index this run computed could not be published.
    W011PublishFailed,
    /// The dirty set exceeded `limits.max_dirty_packages`, so the run
    /// re-indexed the whole repository at head.
    W012ColdRebuild,
    /// A changed path matched an invalidator, so the run is global.
    W013InvalidatorMatched,
    /// The external-repository stage escalated to global.
    W014ExternalsGlobal,
    /// A Starlark build setting a `.bazelrc` edit changed could not be
    /// resolved against the index, so the run is global.
    W015FlagUnresolved,

    // ---- errors ----
    /// A failure with no more specific code: the transparent io, proto,
    /// index and classify wrappers.
    E001Internal,
    /// `cambium.toml` could not be read or does not parse.
    E002Config,
    /// git failed.
    E003Git,
    /// Bazel failed.
    E004Bazel,
    /// A `bazel query` failed.
    E005BazelQuery,
    /// No baseline index is available and `--no-full-build` was given.
    E006BaselineUnavailable,
    /// The index on hand was built with a different `cambium.toml`.
    E007ConfigHashMismatch,
    /// No baseline store is configured.
    E008NoStore,
    /// Refusing to publish an index Bazel could not fully load.
    E009PartialIndex,
    /// The store backend failed.
    E010StoreBackend,
    /// An io error against the store.
    E011StoreIo,
    /// The store backend does not support the operation.
    E012StoreUnsupported,
    /// The command line does not make sense.
    E013Usage,
}

impl Code {
    /// Every code, in numeric order. The documentation test walks this.
    pub const ALL: &'static [Code] = &[
        Code::W001DeprecatedSpelling,
        Code::W002RemovedConfigKey,
        Code::W003IndexConfigMismatch,
        Code::W004BazelrcGlobal,
        Code::W005PackagesTombstoned,
        Code::W006PartialQuery,
        Code::W007StoreFetchFailed,
        Code::W008NoLoadClosure,
        Code::W009WorkspaceLevelBzl,
        Code::W010IncompleteClosure,
        Code::W011PublishFailed,
        Code::W012ColdRebuild,
        Code::W013InvalidatorMatched,
        Code::W014ExternalsGlobal,
        Code::W015FlagUnresolved,
        Code::E001Internal,
        Code::E002Config,
        Code::E003Git,
        Code::E004Bazel,
        Code::E005BazelQuery,
        Code::E006BaselineUnavailable,
        Code::E007ConfigHashMismatch,
        Code::E008NoStore,
        Code::E009PartialIndex,
        Code::E010StoreBackend,
        Code::E011StoreIo,
        Code::E012StoreUnsupported,
        Code::E013Usage,
    ];

    /// The stable string, e.g. `CAMBI-W004`.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::W001DeprecatedSpelling => "CAMBI-W001",
            Code::W002RemovedConfigKey => "CAMBI-W002",
            Code::W003IndexConfigMismatch => "CAMBI-W003",
            Code::W004BazelrcGlobal => "CAMBI-W004",
            Code::W005PackagesTombstoned => "CAMBI-W005",
            Code::W006PartialQuery => "CAMBI-W006",
            Code::W007StoreFetchFailed => "CAMBI-W007",
            Code::W008NoLoadClosure => "CAMBI-W008",
            Code::W009WorkspaceLevelBzl => "CAMBI-W009",
            Code::W010IncompleteClosure => "CAMBI-W010",
            Code::W011PublishFailed => "CAMBI-W011",
            Code::W012ColdRebuild => "CAMBI-W012",
            Code::W013InvalidatorMatched => "CAMBI-W013",
            Code::W014ExternalsGlobal => "CAMBI-W014",
            Code::W015FlagUnresolved => "CAMBI-W015",
            Code::E001Internal => "CAMBI-E001",
            Code::E002Config => "CAMBI-E002",
            Code::E003Git => "CAMBI-E003",
            Code::E004Bazel => "CAMBI-E004",
            Code::E005BazelQuery => "CAMBI-E005",
            Code::E006BaselineUnavailable => "CAMBI-E006",
            Code::E007ConfigHashMismatch => "CAMBI-E007",
            Code::E008NoStore => "CAMBI-E008",
            Code::E009PartialIndex => "CAMBI-E009",
            Code::E010StoreBackend => "CAMBI-E010",
            Code::E011StoreIo => "CAMBI-E011",
            Code::E012StoreUnsupported => "CAMBI-E012",
            Code::E013Usage => "CAMBI-E013",
        }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Code {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Code {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Code::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown diagnostic code {s}")))
    }
}

/// Whether the diagnostic stopped the run.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warning,
    Error,
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Level::Warning => "warning",
            Level::Error => "error",
        })
    }
}

/// Where in the repository a diagnostic is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// Workspace-relative path, as the diff spells it.
    pub path: String,
    /// 1-based line, when the producer knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// One warning or error, in the shape the JSON contract carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: Code,
    pub level: Level,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// What to change: a flag, a config key, a file. Present whenever there
    /// is an action; absent when there is nothing to do but know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn warning(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            level: Level::Warning,
            message: message.into(),
            location: None,
            help: None,
        }
    }

    pub fn error(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            level: Level::Error,
            message: message.into(),
            location: None,
            help: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn at(mut self, path: impl Into<String>, line: Option<u32>) -> Self {
        self.location = Some(Location {
            path: path.into(),
            line,
        });
        self
    }
}

/// The engine's diagnostic sink: a cloneable handle onto one buffer.
///
/// Cloneable because the stages that emit are scattered across the crate and
/// several of them only have a `&self`; the handle is passed around rather
/// than threaded as a `&mut`.
#[derive(Clone, Debug, Default)]
pub struct Diagnostics(Arc<Mutex<Vec<Diagnostic>>>);

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one diagnostic.
    pub fn emit(&self, d: Diagnostic) {
        // A poisoned mutex means another thread panicked while holding it.
        // Losing a diagnostic is not worth a second panic on top of the
        // first, so the lock is recovered rather than unwrapped.
        let mut v = self.0.lock().unwrap_or_else(|e| e.into_inner());
        v.push(d);
    }

    /// Takes everything recorded so far, leaving the sink empty.
    pub fn drain(&self) -> Vec<Diagnostic> {
        let mut v = self.0.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *v)
    }

    /// How many are buffered.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_well_formed() {
        let mut seen = std::collections::BTreeSet::new();
        for c in Code::ALL {
            assert!(seen.insert(c.as_str()), "{c} is assigned twice");
            assert!(
                c.as_str().starts_with("CAMBI-W") || c.as_str().starts_with("CAMBI-E"),
                "{c} is not a CAMBI code"
            );
            assert_eq!(c.as_str().len(), "CAMBI-W000".len(), "{c} is misshapen");
        }
    }

    #[test]
    fn a_code_round_trips_through_json_as_its_string() {
        let d = Diagnostic::warning(Code::W004BazelrcGlobal, "hi").with_help("do that");
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["code"], "CAMBI-W004");
        assert_eq!(json["level"], "warning");
        assert!(json.get("location").is_none(), "absent, not null: {json}");
        let back: Diagnostic = serde_json::from_value(json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn the_sink_is_one_buffer_however_many_handles() {
        let a = Diagnostics::new();
        let b = a.clone();
        a.emit(Diagnostic::warning(Code::W002RemovedConfigKey, "one"));
        b.emit(Diagnostic::warning(Code::W002RemovedConfigKey, "two"));
        assert_eq!(a.len(), 2);
        assert_eq!(b.drain().len(), 2);
        assert!(a.is_empty(), "drain empties the shared buffer");
    }

    /// A code that ships without a row in the table is a code nobody can look
    /// up, and the table is the whole point of having stable numbers.
    #[test]
    fn every_code_is_documented() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/DIAGNOSTICS.md"),
        )
        .expect("docs/DIAGNOSTICS.md should be readable from the engine crate");
        for c in Code::ALL {
            assert!(
                doc.contains(c.as_str()),
                "{c} has no row in docs/DIAGNOSTICS.md; add one saying when it \
                 fires and what to do about it"
            );
        }
    }
}

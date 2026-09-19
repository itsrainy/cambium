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

/// The one table every code lives in: the variant, its doc comment and its
/// wire string. `Code`, [`Code::ALL`] and [`Code::as_str`] are all generated
/// from it, so a code cannot be added to one list without the others.
macro_rules! codes {
    ($( $(#[$doc:meta])* $variant:ident => $wire:literal, )*) => {
        /// A diagnostic's stable identifier.
        ///
        /// **The numbers are assigned once and never reused.** A code that is
        /// retired keeps its variant and its number, and its row in
        /// `docs/DIAGNOSTICS.md` says it is retired; a new diagnostic takes the
        /// next free number. Callers grep for `CAMBI-W004` in stderr and in
        /// JSON, and a number that came to mean something else would break
        /// them silently.
        ///
        /// `Display` is the wire form (`CAMBI-W004`, `CAMBI-E009`) and is what
        /// both the human rendering and the JSON carry.
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Code {
            $( $(#[$doc])* $variant, )*
        }

        impl Code {
            /// Every code, in numeric order. The documentation test walks this.
            pub const ALL: &'static [Code] = &[ $( Code::$variant, )* ];

            /// The stable string, e.g. `CAMBI-W004`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( Code::$variant => $wire, )*
                }
            }
        }
    };
}

codes! {
    // ---- warnings ----
    /// A deprecated spelling was used and still worked: an old verb, an old
    /// flag, or an old value for one. Raised by the CLI's `compat` rewrite.
    W001DeprecatedSpelling => "CAMBI-W001",
    /// `cambium.toml` sets a key this cambium no longer honours.
    W002RemovedConfigKey => "CAMBI-W002",
    /// A stored index was skipped because it was built with a different
    /// `cambium.toml` (or an older rule-hash version).
    W003IndexConfigMismatch => "CAMBI-W003",
    /// A `.bazelrc` edit touched something that could move the graph, so the
    /// run is global.
    W004BazelrcGlobal => "CAMBI-W004",
    /// Dirty packages did not load under `--keep_going` and were tombstoned.
    W005PackagesTombstoned => "CAMBI-W005",
    /// `bazel query //...` finished with errors; the index is partial.
    W006PartialQuery => "CAMBI-W006",
    /// A store fetch command failed with something on stderr, and the
    /// artifact was read as absent.
    W007StoreFetchFailed => "CAMBI-W007",
    /// The base index carries no Skyframe loading layer, so which packages
    /// the change moved cannot be answered and the run is global.
    W008NoLoadingLayer => "CAMBI-W008",
    /// A changed `.bzl` is loaded outside any package, so the run is global.
    W009WorkspaceLevelBzl => "CAMBI-W009",
    /// Retired with the static `.bzl` load closure's witness (format 11): the
    /// loading layer's `BZL_LOAD` edges are Bazel's own, so there is no
    /// stored closure left to be incomplete. The code is not reused.
    W010IncompleteClosure => "CAMBI-W010",
    /// The base index this run computed could not be published.
    W011PublishFailed => "CAMBI-W011",
    /// The dirty set exceeded `limits.max_dirty_packages`, so the run
    /// re-indexed the whole repository at head.
    W012ColdRebuild => "CAMBI-W012",
    /// A changed path matched an invalidator, so the run is global.
    W013InvalidatorMatched => "CAMBI-W013",
    /// The external-repository stage escalated to global.
    W014ExternalsGlobal => "CAMBI-W014",
    /// A Starlark build setting a `.bazelrc` edit changed could not be
    /// resolved against the index, so the run is global.
    W015FlagUnresolved => "CAMBI-W015",
    /// `MODULE.bazel` and a `WORKSPACE` file are both present and nothing on
    /// disk says which one Bazel will honour.
    W016WorkspaceModeAmbiguous => "CAMBI-W016",
    /// The Bazel on PATH is older than the oldest version cambium is tested
    /// against.
    W017BazelVersionUnsupported => "CAMBI-W017",
    /// A stored index is in an older format than this `cambi` reads.
    W018IndexFormatOlder => "CAMBI-W018",
    /// The store holds a lineage built with a different configuration or a
    /// different engine derivation.
    W019LineageConflict => "CAMBI-W019",
    /// `.gitignore` exists and does not mention `cambium.local.toml`.
    W020GitignoreLocalConfig => "CAMBI-W020",
    /// `cambi inspect verify` could not run the `rdeps` query for a sampled
    /// target, and skipped it.
    W021VerifyQueryFailed => "CAMBI-W021",
    /// `bazel mod dump_repo_mapping` succeeded but printed something that is
    /// not a mapping, so the identity mapping stands in for it.
    W022RepoMappingUnparsed => "CAMBI-W022",
    /// A module Bazel autoloads native rules from (`rules_python`,
    /// `protobuf`, ...) moved under a Bazel that autoloads (8 by default, or
    /// 7.4 with the `.bazelrc` flag), so every package using those rules may
    /// have changed and the run is global.
    ///
    /// `CAMBI-W023` is the toolchain rule-class escalation (#26), allocated
    /// on its own branch at the same time as this one; the number is skipped
    /// here so the two cannot collide, and stays skipped if that work lands
    /// under another number, since a number is never reassigned.
    W024AutoloadProvider => "CAMBI-W024",

    // ---- errors ----
    /// A failure with no more specific code: the transparent io, proto,
    /// index and classify wrappers.
    E001Internal => "CAMBI-E001",
    /// `cambium.toml` could not be read or does not parse.
    E002Config => "CAMBI-E002",
    /// git failed.
    E003Git => "CAMBI-E003",
    /// Bazel failed.
    E004Bazel => "CAMBI-E004",
    /// A `bazel query` failed.
    E005BazelQuery => "CAMBI-E005",
    /// No baseline index is available and `--no-full-build` was given.
    E006BaselineUnavailable => "CAMBI-E006",
    /// The index on hand was built with a different `cambium.toml`.
    E007ConfigHashMismatch => "CAMBI-E007",
    /// No baseline store is configured.
    E008NoStore => "CAMBI-E008",
    /// Refusing to publish an index Bazel could not fully load.
    E009PartialIndex => "CAMBI-E009",
    /// The store backend failed.
    E010StoreBackend => "CAMBI-E010",
    /// An io error against the store.
    E011StoreIo => "CAMBI-E011",
    /// The store backend does not support the operation.
    E012StoreUnsupported => "CAMBI-E012",
    /// The command line does not make sense.
    E013Usage => "CAMBI-E013",
    /// `cambium.local.toml` sets a key that belongs in `cambium.toml`.
    E014LocalOverrideKey => "CAMBI-E014",
    /// The Bazel binary could not be found or could not be run.
    E015BazelNotFound => "CAMBI-E015",
    /// A probe object could not be published to the store and fetched back.
    E016StoreRoundTripFailed => "CAMBI-E016",
    /// An index query named a label, package or node kind the index does not
    /// have.
    E017IndexQuery => "CAMBI-E017",
    /// `cambi index update` found that the range invalidates everything, and
    /// was told not to rebuild.
    E018RangeInvalidated => "CAMBI-E018",
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

    /// Takes everything recorded so far and appends it to `out`, once each:
    /// an entry `out` already holds is not repeated. A probe or a plan folds
    /// the sink's findings into a list it has been building itself, and the
    /// same fact can have been observed twice on the way (a tree key found
    /// both directly and through its `commits/` pointer is fetched, and
    /// refused, twice).
    pub fn drain_into(&self, out: &mut Vec<Diagnostic>) {
        extend_unique(out, self.drain());
    }

    /// How many are buffered.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Appends `more` to `out`, skipping every entry `out` already holds.
pub fn extend_unique(out: &mut Vec<Diagnostic>, more: impl IntoIterator<Item = Diagnostic>) {
    for d in more {
        if !out.contains(&d) {
            out.push(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_into_a_list_repeats_nothing() {
        let sink = Diagnostics::new();
        let one = Diagnostic::warning(Code::W003IndexConfigMismatch, "seen twice");
        sink.emit(one.clone());
        sink.emit(one.clone());
        sink.emit(Diagnostic::warning(Code::W007StoreFetchFailed, "once"));
        let mut have = vec![one.clone()];
        sink.drain_into(&mut have);
        assert_eq!(have.len(), 2, "{have:?}");
        assert!(sink.is_empty());
    }

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
